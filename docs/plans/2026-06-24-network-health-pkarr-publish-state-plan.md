# Network-Health pkarr publish-state — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `network_health_snapshot`'s `pkarrStatus.{identityPublished, communityPublishCount, identityLastPublishMs}` reflect real pkarr publish state instead of the hardcoded `false`/`0`/`null` stubs (ZEB-511).

**Architecture:** The `PkarrSnapshot` trait is synchronous; the publisher's handle map is a private `tokio::Mutex` with only an `async` accessor (`active_handles()`). We add a **non-blocking sync accessor** `try_active_handles() -> Option<Vec<String>>` to `PkarrPublisher` upstream (in the `harmony` repo), then derive `identityPublished` (handle set contains `"identity"`) and `communityPublishCount` (count of `"community:"`-prefixed handles) from it. `identityLastPublishMs` is derived in the snapshot synthesis from the already-real `relays[].last_success_ms` (confirmed relay-PUT successes), gated on `identityPublished` so we never claim a publish time for a node that isn't publishing identity.

**Tech Stack:** Rust (`harmony-pkarr` crate + `harmony-app` Tauri backend), TypeScript/Svelte/vitest frontend.

**Decision record (settled with Jake, 2026-06-24):** Mechanism **B — upstream authoritative accessor** (reads the publisher's real state map → drift-free), chosen over a client-side observer mirror. This is a *diagnostics-correctness* ticket, so the snapshot must read the single source of truth and cannot itself drift into lying. Cost accepted: a cross-repo `harmony` PR + a `rev` pin bump here.

## Global Constraints

- **Two repos.** `harmony` (upstream, crate `harmony-pkarr`) at `/Users/zeblith/work/zeblithic/harmony`; `harmony-client` (this repo) at `/Users/zeblith/work/zeblithic/harmony-client`. The harmony-client branch `network-health-pkarr-publish-state` already exists off main `b3497d4d`.
- **Keep Linear IDs out of branch / commit / PR titles.** Reference ZEB-511 in PR *body* only.
- **harmony-pkarr is a pinned git dep** (`Cargo.toml:109` + `:204`, rev `14918226b988034a656aaeffc4b56f36ca72fc9a`). The upstream accessor must be merged (or at least pushed) before harmony-client can pin to it. CI here uses `--locked` + the git rev.
- **Rust gates (run from `src-tauri/`):** `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked -p harmony-app --features test-fixtures` (scoped during dev) → `--all-targets` full sweep before PR. harmony gates run from `crates/harmony-pkarr` or workspace root.
- **Frontend gates (repo root):** `npx tsc --noEmit`; `npx vitest run`.
- **No new DTO/type changes required:** `identityPublished: boolean`, `communityPublishCount: number`, `identityLastPublishMs: number | null` already exist in `src/lib/types/network-health.ts`. The contended-`try_lock` case maps to the conservative default (rare; driver does not hold the lock across network I/O) — documented, not surfaced as a new nullable field.
- **Out of scope:** `recent_fallback_events()` (separate ring-buffer TODO, not named in ZEB-511); `ProdMembership` stub; the publisher wrapper modules (`pkarr_identity_publisher.rs`, `pkarr_community_publisher.rs`) are **not** touched — Option B derives state from handles, not from wrapper-side instrumentation.

---

## Cross-repo sequencing

```
Task 1 (harmony: add accessor) ──push──▶ open harmony PR
        │                                      │
        │ pin harmony-client to BRANCH SHA     │ Jake merges harmony PR
        ▼                                      ▼
Task 2 (harmony-client: rev-bump to branch SHA) ... later re-pin to merged main SHA (Task 6)
        ▼
Tasks 3–5 (rewire ProdPkarrSnapshot + synthesis + frontend) — develop/CI against branch SHA
        ▼
Task 6 (re-pin to merged harmony main SHA once Task 1's PR merges) ──▶ harmony-client PR ready
```

Git deps can pin **any pushed commit SHA**, including a branch HEAD — so harmony-client CI can go green against the unmerged harmony branch, then re-pin to the squash-merge main SHA after the harmony PR merges (Jake's gate).

---

### Task 1: Upstream — `PkarrPublisher::try_active_handles()` (repo: `harmony`)

**Files:**
- Modify: `/Users/zeblith/work/zeblithic/harmony/crates/harmony-pkarr/src/publisher.rs` (add method after `active_handles()` at ~line 132; add test in the existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Produces: `pub fn try_active_handles(&self) -> Option<Vec<String>>` — `Some(handles)` on a successful non-blocking lock, `None` if the state mutex is momentarily contended.

- [ ] **Step 1: Branch off harmony origin/main**

```bash
cd /Users/zeblith/work/zeblithic/harmony
git fetch origin
git log --oneline origin/main -1
# Confirm distance from the currently-pinned rev (informs the Task 6 bump risk):
git log --oneline 14918226b988034a656aaeffc4b56f36ca72fc9a..origin/main -- crates/harmony-pkarr/ | head -30
git checkout -b pkarr-publisher-sync-handle-accessor origin/main
```

- [ ] **Step 2: Write the failing test** (in `mod tests` of `publisher.rs`)

```rust
    /// `try_active_handles` returns the registered handle set without awaiting,
    /// so a synchronous caller (e.g. the Network Health snapshot) can read
    /// publish state. It returns `Some` whenever the state mutex is uncontended.
    #[tokio::test]
    async fn try_active_handles_returns_registered_handles() {
        let relay = MockPkarrRelay::start().await;
        let pool = crate::relay::RelayPool::new(vec![relay.base_url.clone()]);
        let client = Arc::new(crate::relay::RelayClient::new(pool));
        let publisher = Arc::new(PkarrPublisher::new(Arc::clone(&client)));

        // Empty before any registration.
        assert_eq!(publisher.try_active_handles(), Some(Vec::new()));

        let key_builder: EphemeralKeyBuilder =
            Arc::new(move |_at_ms| SigningKey::generate(&mut OsRng));
        let identity_sk = SigningKey::generate(&mut OsRng);
        let mut identity_pub = [0u8; 64];
        identity_pub[32..].copy_from_slice(&identity_sk.verifying_key().to_bytes());
        let id_sk = identity_sk.clone();
        let builder: RecordBuilder = Arc::new(move |at_ms| {
            PkarrRoutingRecord::sign_new(b"blob".to_vec(), identity_pub, at_ms, at_ms + 604_800_000, &id_sk)
                .expect("sign")
        });
        publisher
            .register("identity".to_string(), key_builder, builder)
            .await;

        let handles = publisher.try_active_handles().expect("uncontended");
        assert_eq!(handles, vec!["identity".to_string()]);
    }
```

- [ ] **Step 3: Run it — verify it fails** (method doesn't exist yet)

```bash
cd /Users/zeblith/work/zeblithic/harmony
cargo nextest run -p harmony-pkarr -E 'test(try_active_handles_returns_registered_handles)'
```
Expected: FAIL — `no method named try_active_handles`.

- [ ] **Step 4: Implement** (insert directly after `active_handles()`, ~line 132)

```rust
    /// Non-blocking variant of [`active_handles`][Self::active_handles] for
    /// synchronous callers (e.g. `network_health`'s `PkarrSnapshot`, which is
    /// a sync trait and cannot await). Returns `Some(handles)` when the state
    /// mutex is uncontended, or `None` if it is momentarily locked by the
    /// background driver. The driver never holds this lock across an `await`
    /// (see `drive_pending`: it clones the due set under the lock, then drops
    /// it before each network PUT), so contention windows are sub-millisecond
    /// and `None` is rare — callers treat it as "unknown, fall back".
    pub fn try_active_handles(&self) -> Option<Vec<String>> {
        self.state
            .try_lock()
            .ok()
            .map(|state| state.keys().cloned().collect())
    }
```

- [ ] **Step 5: Run the test — verify it passes**

```bash
cargo nextest run -p harmony-pkarr -E 'test(try_active_handles)'
```
Expected: PASS.

- [ ] **Step 6: Gate + commit + push**

```bash
cd /Users/zeblith/work/zeblithic/harmony
cargo fmt -p harmony-pkarr -- --check
cargo clippy -p harmony-pkarr --all-targets --no-deps -- -D warnings
cargo nextest run -p harmony-pkarr
git add crates/harmony-pkarr/src/publisher.rs
git commit -m "feat(pkarr): non-blocking try_active_handles() sync accessor

Synchronous callers (network_health's PkarrSnapshot) need to read the
registered-handle set without awaiting. Adds try_active_handles() backed
by Mutex::try_lock(); returns None only under the (sub-ms) driver lock
window.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_012opk5CDiSc47nBcxaADfsX"
git push -u origin pkarr-publisher-sync-handle-accessor
git rev-parse HEAD   # record this SHA for Task 2
```

- [ ] **Step 7: Open the harmony PR**

```bash
gh pr create --repo zeblithic/harmony --head pkarr-publisher-sync-handle-accessor \
  --title "pkarr: non-blocking try_active_handles() sync accessor" \
  --body "<body referencing ZEB-511 as the consumer; see plan>"
```

---

### Task 2: Pin harmony-client to the harmony branch SHA (repo: `harmony-client`)

**Files:**
- Modify: `src-tauri/Cargo.toml:109` and `:204` (both `harmony-pkarr` git rev lines)
- Modify: `src-tauri/Cargo.lock` (regenerated)

**Interfaces:**
- Consumes: the Task 1 branch HEAD SHA (`git rev-parse HEAD` output).

- [ ] **Step 1: Confirm on the harmony-client branch**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git branch --show-current   # expect: network-health-pkarr-publish-state
```

- [ ] **Step 2: Bump both rev pins to the Task-1 branch SHA**

Replace `14918226b988034a656aaeffc4b56f36ca72fc9a` with `<TASK1_SHA>` on both `src-tauri/Cargo.toml:109` and `:204`.

- [ ] **Step 3: Regenerate the lock + verify the new accessor resolves**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo update -p harmony-pkarr --precise <TASK1_SHA> 2>/dev/null || cargo build -p harmony-app
cargo build -p harmony-app   # must compile against the new dep graph
```
Expected: builds clean; `Cargo.lock` now references `<TASK1_SHA>`.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "build: bump harmony-pkarr to try_active_handles() rev

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_012opk5CDiSc47nBcxaADfsX"
```

---

### Task 3: Rewire `ProdPkarrSnapshot` to read real handle state (repo: `harmony-client`)

**Files:**
- Modify: `src-tauri/src/network_health.rs` — `ProdPkarrSnapshot::{identity_published, community_publish_count}` (~1471-1486); add tests in `#[cfg(test)] mod tests`.

**Interfaces:**
- Consumes: `self.publisher.try_active_handles() -> Option<Vec<String>>` (Task 1).
- `identity_last_publish_ms()` stays `None` in this impl — it is derived in the synthesis (Task 4), not here.

- [ ] **Step 1: Write failing tests** (add to `mod tests`)

```rust
    fn prod_pkarr_with_handles(handles: &[&str]) -> impl std::future::Future<Output = ProdPkarrSnapshot> + '_ {
        async move {
            use harmony_pkarr::{
                current_epoch_id, derive_ephemeral_key, testing::MockPkarrRelay, EphemeralKeyBuilder,
                PkarrCase, PkarrPublisher, PkarrRoutingRecord, RecordBuilder, RelayClient, RelayPool,
            };
            let relay = MockPkarrRelay::start().await;
            let pool = RelayPool::new(vec![relay.base_url.clone()]);
            let client = std::sync::Arc::new(RelayClient::new(pool));
            let publisher = std::sync::Arc::new(PkarrPublisher::new(std::sync::Arc::clone(&client)));
            let id_sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
            let mut id_pub = [0u8; 64];
            id_pub[32..].copy_from_slice(&id_sk.verifying_key().to_bytes());
            for h in handles {
                let id_pub_k = id_pub;
                let kb: EphemeralKeyBuilder = std::sync::Arc::new(move |at_ms| {
                    derive_ephemeral_key(PkarrCase::Identity, &id_pub_k, &current_epoch_id(at_ms).to_be_bytes())
                });
                let sk = id_sk.clone();
                let b: RecordBuilder = std::sync::Arc::new(move |at_ms| {
                    PkarrRoutingRecord::sign_new(b"x".to_vec(), id_pub, at_ms, at_ms + 604_800_000, &sk).expect("sign")
                });
                publisher.register((*h).to_string(), kb, b).await;
            }
            ProdPkarrSnapshot::new(publisher)
        }
    }

    #[tokio::test]
    async fn prod_pkarr_identity_published_reflects_registered_handle() {
        let snap = prod_pkarr_with_handles(&["identity"]).await;
        assert!(snap.identity_published());
        assert_eq!(snap.community_publish_count(), 0);
    }

    #[tokio::test]
    async fn prod_pkarr_community_count_counts_community_handles() {
        let snap = prod_pkarr_with_handles(&["identity", "community:aa", "community:bb"]).await;
        assert!(snap.identity_published());
        assert_eq!(snap.community_publish_count(), 2);
    }

    #[tokio::test]
    async fn prod_pkarr_identity_unpublished_when_no_identity_handle() {
        let snap = prod_pkarr_with_handles(&["community:aa"]).await;
        assert!(!snap.identity_published());
        assert_eq!(snap.community_publish_count(), 1);
    }
```

- [ ] **Step 2: Run — verify they fail** (current stub returns `false`/`0`)

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(prod_pkarr_identity_published_reflects_registered_handle) + test(prod_pkarr_community_count_counts_community_handles) + test(prod_pkarr_identity_unpublished_when_no_identity_handle)'
```
Expected: FAIL (asserts `true`/`2`/`1` but stub yields `false`/`0`).

- [ ] **Step 3: Implement** (replace the two stub bodies; keep `identity_last_publish_ms` → `None`)

```rust
    fn identity_published(&self) -> bool {
        // Real state via the non-blocking publisher accessor (ZEB-511). The
        // identity case-B publication registers under the fixed "identity"
        // handle (see pkarr_identity_publisher::HANDLE). `None` only on the
        // sub-ms driver lock window → treat as "not observed" (conservative).
        self.publisher
            .try_active_handles()
            .map(|h| h.iter().any(|k| k == "identity"))
            .unwrap_or(false)
    }
    fn identity_last_publish_ms(&self) -> Option<u64> {
        // Derived in the snapshot synthesis from the confirmed relay
        // last_success_ms (ZEB-511); the publisher itself records no
        // last-publish wall-clock. See NetworkHealthService::snapshot.
        None
    }
    fn community_publish_count(&self) -> u32 {
        // Case-C community publications register under "community:<hex>"
        // handles (see pkarr_community_publisher). Count them (ZEB-511).
        self.publisher
            .try_active_handles()
            .map(|h| h.iter().filter(|k| k.starts_with("community:")).count() as u32)
            .unwrap_or(0)
    }
```

- [ ] **Step 4: Run — verify they pass**

```bash
cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(prod_pkarr)'
```
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/network_health.rs
git commit -m "fix(network-health): real identityPublished + communityPublishCount

ProdPkarrSnapshot reads the publisher's registered handles via the new
non-blocking try_active_handles(); derives identityPublished from the
\"identity\" handle and communityPublishCount from \"community:\" handles,
instead of always-false/always-0 stubs.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_012opk5CDiSc47nBcxaADfsX"
```

---

### Task 4: Derive `identityLastPublishMs` from confirmed relay successes (repo: `harmony-client`)

**Files:**
- Modify: `src-tauri/src/network_health.rs` — `NetworkHealthService::snapshot()` `PkarrHealthSummary` assembly (~622-633); add a test.

**Interfaces:**
- Consumes: `self.relay.relay_health() -> Vec<harmony_pkarr::RelayHealth>` (each `RelayHealth.last_success_ms: Option<u64>`); `self.pkarr.identity_published()`.
- Produces: `identity_last_publish_ms = identity_published ? max(relays[].last_success_ms) : None`.

- [ ] **Step 1: Refactor the struct-literal to build `relays` first, then derive the timestamp**

Replace the `pkarr_status: PkarrHealthSummary { ... }` block (~622-633) with:

```rust
            pkarr_status: {
                let relays: Vec<RelayHealthWire> = self
                    .relay
                    .relay_health()
                    .into_iter()
                    .map(Into::into)
                    .collect();
                let identity_published = self.pkarr.identity_published();
                // ZEB-511: the publisher records no last-publish wall-clock,
                // but relay health does (last_success_ms, a confirmed PUT
                // success). Surface the most-recent confirmed success — but
                // only when we are actually publishing identity, so we never
                // attribute a community/friend PUT's timestamp to an identity
                // that isn't being published. Falls back to the impl's own
                // (currently None) value when not publishing.
                let identity_last_publish_ms = if identity_published {
                    relays
                        .iter()
                        .filter_map(|r| r.last_success_ms)
                        .max()
                        .or_else(|| self.pkarr.identity_last_publish_ms())
                } else {
                    self.pkarr.identity_last_publish_ms()
                };
                PkarrHealthSummary {
                    identity_published,
                    identity_last_publish_ms,
                    community_publish_count: self.pkarr.community_publish_count(),
                    recent_fallback_events: self.pkarr.recent_fallback_events(),
                    relays,
                }
            },
```

(Confirm the `RelayHealthWire` field name during implementation — it is `last_success_ms` per `network_health.rs:110`.)

- [ ] **Step 2: Write a test** proving the derivation (use the existing `NetworkHealthService` test harness with fake sources). Add to `mod tests`:

```rust
    #[tokio::test]
    async fn snapshot_identity_last_publish_ms_from_relay_success_when_publishing() {
        // FakePkarr: identity_published=true, community_count=0, own ts=None.
        // FakeRelay: two relays, last_success_ms = Some(1000) and Some(3000).
        // Expect identity_last_publish_ms == Some(3000).
        // (Build via the existing fake-source pattern used by other
        //  NetworkHealthService::snapshot tests in this module.)
    }

    #[tokio::test]
    async fn snapshot_identity_last_publish_ms_null_when_not_publishing() {
        // identity_published=false → identity_last_publish_ms falls back to
        // the impl's None even if relays report successes.
    }
```

Flesh these out against the module's existing fake `PkarrSnapshot`/relay-source test scaffolding (mirror whatever `snapshot()` tests already exist; if none use a fake relay source, introduce a minimal one alongside `FakeMembership`).

- [ ] **Step 3: Run — verify fail then implement-to-pass, then full module run**

```bash
cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(snapshot_identity_last_publish_ms)'
cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(network_health)'
```
Expected: target tests PASS; no regressions in the module.

- [ ] **Step 4: Gate + commit**

```bash
cargo fmt --all -- --check
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
git add src/network_health.rs
git commit -m "fix(network-health): derive identityLastPublishMs from relay success

The pkarr publisher records no last-publish wall-clock; relay health does
(last_success_ms). Surface the most-recent confirmed relay PUT success as
identityLastPublishMs, gated on identityPublished so a non-publishing node
never reports a spurious timestamp.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_012opk5CDiSc47nBcxaADfsX"
```

---

### Task 5: Frontend sanity — assert the panel renders truthful state (repo: `harmony-client`)

**Files:**
- Modify: `src/lib/components/__tests__/NetworkHealthView.test.ts` (add a case where `identityPublished: true` / `communityPublishCount > 0` / `identityLastPublishMs: <number>` renders correctly).

**Interfaces:** No DTO type change — `src/lib/types/network-health.ts` already types all three fields.

- [ ] **Step 1: Add a "published" render assertion** mirroring the existing test fixtures (the file already builds snapshots with these fields at lines 29-31/116-118/181). Add one fixture with `identityPublished: true`, `communityPublishCount: 2`, `identityLastPublishMs: 1_700_000_000_000` and assert the panel shows the published indicator + count (match the component's actual rendered text/testids).

- [ ] **Step 2: Run frontend gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit
npx vitest run src/lib/components/__tests__/NetworkHealthView.test.ts
```
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add src/lib/components/__tests__/NetworkHealthView.test.ts
git commit -m "test(network-health): cover the published-state panel render

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_012opk5CDiSc47nBcxaADfsX"
```

---

### Task 6: Re-pin to merged harmony main + final sweep + PR (repo: `harmony-client`)

**Gated on:** Jake merging the Task-1 harmony PR. (Human-in-loop: do not self-merge upstream.)

- [ ] **Step 1: After the harmony PR merges, re-pin both revs to the new harmony `origin/main` SHA**

```bash
cd /Users/zeblith/work/zeblithic/harmony && git fetch origin && git rev-parse origin/main   # = <MERGED_SHA>
# In harmony-client: replace <TASK1_SHA> with <MERGED_SHA> on Cargo.toml:109 + :204, then:
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo build -p harmony-app
git add Cargo.toml Cargo.lock && git commit -m "build: re-pin harmony-pkarr to merged main"
```
If `origin/main` has drifted far beyond the old pin, scan `git log 14918226..origin/main -- crates/harmony-pkarr/` for behavior-affecting changes and note them in the PR body.

- [ ] **Step 2: Full gate sweep**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked -p harmony-app --features test-fixtures
cd /Users/zeblith/work/zeblithic/harmony-client && npx tsc --noEmit && npx vitest run
```
Expected: all green.

- [ ] **Step 3: Push + open the harmony-client PR**

```bash
git push -u origin network-health-pkarr-publish-state
gh pr create --repo zeblithic/harmony-client --head network-health-pkarr-publish-state \
  --title "Network Health: real pkarr publish state (identityPublished / count / last-publish)" \
  --body "<references ZEB-511 + the merged harmony PR; summarizes the two-repo change>"
```

- [ ] **Step 4: Bot review loop** (Qodo + CodeAnt first pass → address all → one CodeRabbit final via `@coderabbitai review`). Hold at Jake's merge gate; never self-merge.

---

## Self-Review

- **Spec coverage:** ZEB-511 names three fields — `identityPublished` (Task 3), `communityPublishCount` (Task 3), `identityLastPublishMs` (Task 4). The "prefer null/unknown over confident-false" note is honored: `identityPublished`/count read the real registered set (false only on true sub-ms lock contention, not always); `identityLastPublishMs` is `null` unless a confirmed relay success exists *and* we're publishing identity. ✓
- **Placeholder scan:** Task 4 Step 2 leaves the two synthesis-test bodies to be fleshed against the module's existing fake-source scaffolding — flagged explicitly because the exact fake-source shape must be read at implementation time (the module may or may not already have a fake relay source). All other steps carry complete code. The implementer must read the surrounding `mod tests` before writing them.
- **Type consistency:** `try_active_handles() -> Option<Vec<String>>` (Task 1) is consumed identically in Task 3. `RelayHealthWire.last_success_ms: Option<u64>` (existing) feeds Task 4. Frontend DTO types already match (Task 5). ✓
- **Cross-repo risk:** captured in the sequencing diagram + Task 6 drift-scan. ✓
