# ZEB-323 Phase 2b: harmony-client pkarr policies + IPCs + UX Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the `harmony-pkarr` primitive (shipped in Phase 2a, harmony PR #270) into harmony-client with three case-specific policies (A invite-redemption, B opt-in identity-keyed, C in-community fallback), 5 new Tauri IPCs, 3 new events, and 3 small UX additions. Ships the user-visible half of cross-WAN first-contact discovery.

**Architecture:** Five new policy modules in `src-tauri/src/` plus a small additive change to Phase 1's `ReachabilityResolver` (new async `resolve_async` that falls back to pkarr; existing sync `resolve` unchanged). Five IPCs added to lib.rs. Three Svelte UX deltas. Three integration tests + one wire-format pin.

**Tech Stack:** Rust (Tauri backend), Svelte 5 (frontend), `harmony-pkarr` (new harmony-core dep, pinned to PR #270's branch initially; updated to merge SHA before this PR merges).

**Spec:** `docs/specs/2026-05-23-zeb-321-phase2-discovery-bootstrap-design.md` (commit `cb5cca5`), Sections 4.2/4.3/4.4, 6, 7.2/7.3/7.4, 11, 12, 13.2/13.3, 14. **Linear:** ZEB-323. **Branch:** `zeb-323-harmony-client-pkarr-phase-2b` (created off `origin/main` `cb5cca5`).

**HARD RULES from user memory (every implementer subagent must enforce):**
- 5 backend gates from `src-tauri/`: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (the harmony-client CLAUDE.md confirms `cargo nextest` is the right runner; `--all-targets` + `--features test-fixtures` + `--locked` are load-bearing).
- 2 frontend gates from repo root: `npx tsc --noEmit`, `npx vitest run` (NOT pnpm).
- harmony-client CI is **disabled** (per `feedback_ci_disabled` — `ci.yml.disabled`). Pretend CI is green; bots still review.
- Tauri IPC: `rename_all = "snake_case"` on commands (Rust `snake_case` ↔ JS `camelCase`).
- Tauri error extraction (frontend): `e instanceof Error ? e.message : String(e)`.
- Implementer gate budget per `feedback_implementer_gate_time_budget`: commit-before-gate + 10-min wall-clock kill switch (Bash tool `timeout` param up to 600000) + DONE_WITH_CONCERNS escape hatch.
- Long-running background per `feedback_long_running_background_supervision`: any cargo > 10 min MUST use foreground timeout or ScheduleWakeup heartbeat. macOS XprotectService can hang first-run nextest; per CLAUDE.md, dev tools enabled per Jake's machine.
- `cargo fmt` MUST be in implementer verification, not just clippy.
- Pipe exit codes: `set -o pipefail` or `${PIPESTATUS[0]}` when piping cargo output through tail/grep.
- No worktrees; git checkout in main repo only.
- Pre-existing orphan failures captured in Task 0 baseline are NOT blocking; new failures introduced by this PR ARE blocking.
- Second-order correctness review (`feedback_second_order_correctness_review`): when extending Phase 1's resolver, enumerate every reader of dispatch-state fields being modified.

**Cross-repo coordination:**
- PR 1 (harmony#270) must merge BEFORE this PR can merge.
- This PR's `src-tauri/Cargo.toml` initially pins `harmony-pkarr` to the `zeb-322-harmony-pkarr-crate` git BRANCH ref of `zeblithic/harmony`. After PR #270 merges, update the pin to the merge commit SHA before this PR merges.

## File Structure

```
src-tauri/Cargo.toml                                    # MODIFY — add harmony-pkarr dep (git ref → merge SHA later)
src-tauri/src/
├─ lib.rs                                               # MODIFY — register 5 new IPCs + 3 events + wire pkarr publisher/resolver at boot
├─ reachability_resolver.rs                             # MODIFY — add ReachabilityFallback trait + fallback_source field + resolve_async()
├─ pkarr_settings.rs                                    # NEW — persisted opt-in for case B
├─ pkarr_resolver_adapter.rs                            # NEW — case C fallback (impls ReachabilityFallback)
├─ pkarr_invite_publisher.rs                            # NEW — case A lifecycle
├─ pkarr_identity_publisher.rs                          # NEW — case B lifecycle
└─ pkarr_community_publisher.rs                         # NEW — case C lifecycle

src-tauri/tests/
├─ pkarr_invite_redemption_integration.rs              # NEW
├─ pkarr_identity_discovery_integration.rs             # NEW
├─ pkarr_community_fallback_integration.rs             # NEW
└─ wire_format_pkarr_routing_record_fixtures.rs        # NEW (mirrors Phase 1's wire-format-pin pattern)

src/lib/types/connectivity.ts                           # MODIFY — extend with DiscoveredRecord type
src/lib/connectivity-adapter.ts                         # MODIFY — wrap 5 new IPCs + 3 new events
src/lib/components/
├─ RedeemInviteDialog.svelte                            # MODIFY — wire new IPC + progress stages
├─ Settings.svelte (or similar)                         # MODIFY — add Network Discoverability toggle
└─ DiagnosticsPanel.svelte                              # MODIFY — add "Network Discovery (pkarr)" section
```

---

## Task 0: Pre-flight baseline (no commit)

**Files:** none modified.

- [ ] **Step 1: Verify branch state**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git status --short    # only untracked .DS_Store files OK
git rev-parse HEAD    # cb5cca5 (Phase 2 spec)
git rev-parse --abbrev-ref HEAD  # zeb-323-harmony-client-pkarr-phase-2b
```

- [ ] **Step 2: Capture backend baseline**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast 2>&1 | tee /tmp/zeb-323-backend-baseline.log | tail -30
```

Bash tool `timeout: 900000` (15 min — harmony-client has multiple integration test binaries; first run may be slow due to XprotectService scanning per the CLAUDE.md note; subsequent runs are fast). Capture the pass/fail counts. Per user memory `feedback_test_drift_is_our_fault`, pre-existing failures (folder_ingest, mint, mint_sync, folder_ingest_walker_integration, rename_content_integration, ~27 known orphans) are NOT blocking — record them in `/tmp/zeb-323-baseline-failures.txt`.

- [ ] **Step 3: Capture frontend baseline**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit 2>&1 | tail -10
npx vitest run 2>&1 | tail -20
```

Both should pass on `origin/main` `cb5cca5`. If either fails, STOP and escalate (it's a pre-existing problem).

- [ ] **Step 4: Verify Phase 1 surface is what we expect**

```bash
grep -n "pub fn\|pub async fn" /Users/zeblith/work/zeblithic/harmony-client/src-tauri/src/reachability_resolver.rs
```

Expected output includes: `new`, `update(actor, payload, hlc)`, `resolve(&self, actor) -> Vec<ReachabilityAnnouncePayload>`, `list_active_peers`, `resolve_by_node_id`, `remove_owner`. If any expected method is missing or has a different signature, flag it in Task 1.

**No commit.**

---

## Task 1: Wire harmony-pkarr dep + initial scaffold

**Purpose:** Add the `harmony-pkarr` git dep to `src-tauri/Cargo.toml` and confirm the workspace builds against it. No source changes yet beyond the dep wiring — just verifies the cross-repo connection works.

**Files:**
- Modify: `src-tauri/Cargo.toml` — add `harmony-pkarr = { git = "https://github.com/zeblithic/harmony", branch = "zeb-322-harmony-pkarr-crate" }` (with `features = ["test-fixtures"]` initially so we can use the mock relay in integration tests later)

- [ ] **Step 1: Add the dep**

Edit `src-tauri/Cargo.toml`. Insert `harmony-pkarr = { git = "https://github.com/zeblithic/harmony", branch = "zeb-322-harmony-pkarr-crate", features = ["test-fixtures"] }` into `[dependencies]` (alphabetical placement near other `harmony-*` deps if any, else near other `git` deps).

NOTE for later: when PR #270 merges, this will be updated to `rev = "<merge-commit-sha>"` in Task 11 before final PR merge.

- [ ] **Step 2: Verify compile**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo check 2>&1 | tail -10
```

Bash tool `timeout: 600000` (10 min — first-time fetch of harmony git repo can take a while). Expected: `Finished `dev` profile`. If git auth fails for a public repo, document the issue.

- [ ] **Step 3: Verify use-able from harmony-client code**

Create a temporary throwaway test (won't commit) — add to `src-tauri/src/lib.rs`:
```rust
#[cfg(test)]
mod _pkarr_dep_smoke {
    use harmony_pkarr::{PkarrCase, derive_ephemeral_key};

    #[test]
    fn dep_is_wired() {
        let _key = derive_ephemeral_key(PkarrCase::Invite, &[0u8; 64], &[0u8; 8]);
    }
}
```

Run: `cargo nextest run --locked -p harmony-app dep_is_wired 2>&1 | tail -5`. Expect pass. Then DELETE this test module before commit (it's smoke; later tasks use harmony-pkarr properly).

- [ ] **Step 4: Per-crate gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -10
```

Both must pass.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "feat(zeb-323): add harmony-pkarr git dep (pinned to zeb-322 branch)"
```

---

## Task 2: Reachability resolver extension (Phase 1 surgical change)

**Purpose:** Add a `ReachabilityFallback` async trait + `fallback_source` field + new `resolve_async()` method to Phase 1's resolver. Existing sync `resolve()` unchanged so existing callers (zenoh transport, IPC handlers) keep working. The async variant is what new pkarr-aware callers use.

**Files:**
- Modify: `src-tauri/src/reachability_resolver.rs`

- [ ] **Step 1: Read existing resolver + identify call sites**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
grep -rn "reachability_resolver::\|ReachabilityResolver" src/ tests/ 2>&1 | head -20
```

Note every existing caller of `resolve()`. They must continue to work unchanged.

- [ ] **Step 2: Write the failing test first**

In `src-tauri/src/reachability_resolver.rs` tests module, add:
```rust
#[cfg(test)]
mod fallback_tests {
    use super::*;
    use crate::owner_state_types::OwnerAddr;
    use async_trait::async_trait;
    use std::sync::Arc;

    struct StubFallback {
        responses: std::sync::Mutex<Vec<ReachabilityAnnouncePayload>>,
    }

    #[async_trait]
    impl ReachabilityFallback for StubFallback {
        async fn resolve(&self, _addr: &OwnerAddr) -> Vec<ReachabilityAnnouncePayload> {
            self.responses.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn resolve_async_returns_empty_when_no_fallback_and_no_cached() {
        let r = ReachabilityResolver::new();
        let addr = OwnerAddr([0u8; 32]);
        let out = r.resolve_async(&addr).await;
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn resolve_async_falls_back_to_pkarr_on_cache_miss() {
        let r = ReachabilityResolver::new();
        let addr = OwnerAddr([0u8; 32]);
        let stub_payload = // ... build a ReachabilityAnnouncePayload fixture
            ReachabilityAnnouncePayload { /* match Phase 1's shape */ };
        let stub = Arc::new(StubFallback {
            responses: std::sync::Mutex::new(vec![stub_payload.clone()]),
        });
        r.set_fallback_source(stub);

        let out = r.resolve_async(&addr).await;
        assert_eq!(out.len(), 1);
        // Subsequent sync resolve hits warm cache
        let warm = r.resolve(&addr);
        assert_eq!(warm.len(), 1);
    }
}
```

(Implementer: build the `ReachabilityAnnouncePayload` fixture by reading the existing Phase 1 test code in this same file — there should be a `fn fixture_payload(...)` or similar pattern already present. Reuse it.)

- [ ] **Step 3: Run tests to verify they fail**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked -p harmony-app fallback_tests 2>&1 | tail -10
```

Expected: compile errors (the trait + field + method + setter don't exist yet).

- [ ] **Step 4: Add the trait + field + method**

In `reachability_resolver.rs`:

```rust
use async_trait::async_trait;
use std::sync::Arc;

#[async_trait]
pub trait ReachabilityFallback: Send + Sync {
    async fn resolve(&self, addr: &OwnerAddr) -> Vec<ReachabilityAnnouncePayload>;
}
```

Add to `ReachabilityResolver` struct:
```rust
fallback_source: std::sync::RwLock<Option<Arc<dyn ReachabilityFallback>>>,
```

Initialize in `new()`:
```rust
fallback_source: std::sync::RwLock::new(None),
```

Add setter:
```rust
pub fn set_fallback_source(&self, fb: Arc<dyn ReachabilityFallback>) {
    *self.fallback_source.write().expect("fallback_source poisoned") = Some(fb);
}
```

Add async resolver:
```rust
pub async fn resolve_async(&self, addr: &OwnerAddr) -> Vec<ReachabilityAnnouncePayload> {
    // 1. Sync cache check (existing resolve() semantics).
    let cached = self.resolve(addr);
    if !cached.is_empty() {
        return cached;
    }
    // 2. Fallback to pkarr if configured.
    let fb = {
        let guard = self.fallback_source.read().expect("fallback_source poisoned");
        guard.clone()
    };
    let Some(fb) = fb else { return Vec::new() };
    let payloads = fb.resolve(addr).await;
    // 3. Populate cache so subsequent sync resolves hit warm.
    for payload in &payloads {
        // Use the existing update() with the payload's announced_at_ms as HLC wall.
        let hlc = Hlc::from_wall_ms(payload.announced_at_ms); // adapt to actual Hlc constructor
        self.update(*addr, payload.clone(), hlc);
    }
    payloads
}
```

(Implementer: adapt the `Hlc::from_wall_ms` to whatever the actual Phase 1 HLC constructor looks like — search for `Hlc::` usage in this file or `owner_state_types.rs`. The key correctness property is that the fallback-populated entry should be SUBORDINATE to any CRDT-sourced entry with a higher HLC, which Phase 1's existing LWW logic already handles.)

Add to `src-tauri/Cargo.toml` `[dependencies]` if not present: `async-trait = "0.1"` (workspace-true if present).

- [ ] **Step 5: Run tests + verify**

```bash
cargo nextest run --locked -p harmony-app fallback_tests 2>&1 | tail -10
# All 2 new tests must pass.
# Also re-run the existing reachability_resolver:: tests to confirm no regressions:
cargo nextest run --locked -p harmony-app reachability_resolver 2>&1 | tail -10
```

- [ ] **Step 6: Per-task gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -10
```

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/reachability_resolver.rs src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "feat(zeb-323): ReachabilityResolver fallback_source + resolve_async (Phase 1 additive)"
```

---

## Task 3: pkarr_settings — persisted opt-in for case B

**Purpose:** Tiny module that reads/writes the case-B "make me discoverable" toggle. Persists to `<app_data_dir>/connectivity-settings.json`.

**Files:**
- Create: `src-tauri/src/pkarr_settings.rs`
- Modify: `src-tauri/src/lib.rs` — `pub mod pkarr_settings;`

- [ ] **Step 1: Write failing tests + implementation**

Create `src-tauri/src/pkarr_settings.rs`:

```rust
//! Persisted user preferences for Phase 2 pkarr policies.
//!
//! Only case B (opt-in identity-keyed discoverability) needs persistence today.
//! Lives at `<app_data_dir>/connectivity-settings.json`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PkarrSettings {
    /// Case B (identity-keyed discoverability) — opt-in, default OFF.
    #[serde(default)]
    pub identity_discoverable: bool,
}

impl PkarrSettings {
    pub fn load_or_default(path: &PathBuf) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &PathBuf) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        std::fs::write(path, json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn defaults_to_not_discoverable() {
        let settings = PkarrSettings::default();
        assert!(!settings.identity_discoverable);
    }

    #[test]
    fn load_missing_file_returns_default() {
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("nonexistent.json");
        let settings = PkarrSettings::load_or_default(&path);
        assert!(!settings.identity_discoverable);
    }

    #[test]
    fn round_trip_save_then_load() {
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("connectivity-settings.json");
        let mut settings = PkarrSettings::default();
        settings.identity_discoverable = true;
        settings.save(&path).expect("save");

        let loaded = PkarrSettings::load_or_default(&path);
        assert_eq!(loaded, settings);
    }

    #[test]
    fn load_corrupted_file_returns_default() {
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("bad.json");
        std::fs::write(&path, "not json {{").expect("write");
        let settings = PkarrSettings::load_or_default(&path);
        assert!(!settings.identity_discoverable);
    }
}
```

Verify `tempfile` is in `src-tauri/Cargo.toml` `[dev-dependencies]`. If not, add `tempfile = "3"`.

Add `pub mod pkarr_settings;` to `src-tauri/src/lib.rs` (alphabetical position near other modules).

- [ ] **Step 2: Run tests + gates + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked -p harmony-app pkarr_settings 2>&1 | tail -10  # 4 tests pass
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
git add src-tauri/src/pkarr_settings.rs src-tauri/src/lib.rs src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "feat(zeb-323): PkarrSettings — persisted opt-in for case B"
```

---

## Task 4: pkarr_resolver_adapter — case C ReachabilityFallback impl

**Purpose:** Implement Phase 1's new `ReachabilityFallback` trait by wrapping `harmony_pkarr::PkarrResolver`. For a given peer addr, iterate every community this device is in, derive case-C key per community, query pkarr-relay, return decoded routing payloads.

**Files:**
- Create: `src-tauri/src/pkarr_resolver_adapter.rs`
- Modify: `src-tauri/src/lib.rs` — `pub mod pkarr_resolver_adapter;`

- [ ] **Step 1: Write the module**

Create `src-tauri/src/pkarr_resolver_adapter.rs`:

```rust
//! Case C fallback (in-community pkarr lookup) — implements Phase 1's
//! `ReachabilityFallback` trait by querying pkarr-relays for peer routing.
//!
//! Triggered automatically by `ReachabilityResolver::resolve_async()` when
//! the in-memory CRDT map has no fresh entry for the requested peer.

use async_trait::async_trait;
use ed25519_dalek::SigningKey;
use harmony_pkarr::{derive_ephemeral_key, current_epoch_id, epoch_tolerance_window, PkarrCase, PkarrResolver};
use std::sync::Arc;

use crate::owner_state_types::OwnerAddr;
use crate::reachability_record::ReachabilityAnnouncePayload;
use crate::reachability_resolver::ReachabilityFallback;

/// Wraps `harmony_pkarr::PkarrResolver` and a closure that produces the set of
/// (community_id, EpochKey, target_member_identity_pub) tuples a seeker should
/// try for a given peer address. The closure is plumbed in from lib.rs which
/// has access to NodeState's community list and per-community EpochKey.
pub struct PkarrResolverAdapter {
    pkarr: Arc<PkarrResolver>,
    contexts: Arc<dyn Fn(&OwnerAddr) -> Vec<PkarrCommunityContext> + Send + Sync>,
}

#[derive(Clone)]
pub struct PkarrCommunityContext {
    pub community_id: crate::owner_state_types::SpaceId,
    pub epoch_key: [u8; 32],
    pub target_member_identity_pub: [u8; 64],
}

impl PkarrResolverAdapter {
    pub fn new(
        pkarr: Arc<PkarrResolver>,
        contexts: Arc<dyn Fn(&OwnerAddr) -> Vec<PkarrCommunityContext> + Send + Sync>,
    ) -> Self {
        Self { pkarr, contexts }
    }
}

#[async_trait]
impl ReachabilityFallback for PkarrResolverAdapter {
    async fn resolve(&self, addr: &OwnerAddr) -> Vec<ReachabilityAnnouncePayload> {
        let ctxs = (self.contexts)(addr);
        if ctxs.is_empty() {
            return Vec::new();
        }

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time")
            .as_millis() as u64;
        let epoch_window = epoch_tolerance_window(now_ms);

        // For each (community_context × epoch), derive the key, query pkarr,
        // collect successful records. First valid response per addr wins.
        let mut payloads = Vec::new();
        for ctx in ctxs {
            for epoch in epoch_window {
                let mut info = Vec::with_capacity(64 + 8);
                info.extend_from_slice(&ctx.target_member_identity_pub);
                info.extend_from_slice(&epoch.to_be_bytes());
                let signing = derive_ephemeral_key(PkarrCase::Community, &ctx.epoch_key, &info);
                let verifying = signing.verifying_key();
                if let Ok(Some(rec)) = self.pkarr.resolve(&verifying).await {
                    // RPK2 + RPK3: verify inner sig + identity match
                    if rec.verify_inner_sig().is_err() {
                        continue;
                    }
                    if rec.verify_identity_match(&ctx.target_member_identity_pub).is_err() {
                        continue;
                    }
                    if rec.verify_skew(now_ms).is_err() {
                        continue;
                    }
                    // Decode routing_blob into harmony-client's ReachabilityAnnouncePayload.
                    if let Ok(payload) = ciborium::from_reader::<ReachabilityAnnouncePayload, _>(rec.routing_blob.as_slice()) {
                        payloads.push(payload);
                        break; // First valid per community
                    }
                }
            }
        }
        payloads
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harmony_pkarr::{PkarrRoutingRecord, PkarrPublisher, RelayClient, RelayPool, testing::MockPkarrRelay};
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    fn build_identity_pub(sk: &SigningKey) -> [u8; 64] {
        let mut out = [0u8; 64];
        out[32..].copy_from_slice(&sk.verifying_key().to_bytes());
        out
    }

    #[tokio::test]
    async fn empty_contexts_returns_empty() {
        let relay = MockPkarrRelay::start().await;
        let pkarr = Arc::new(PkarrResolver::new(Arc::new(RelayClient::new(
            RelayPool::new(vec![relay.base_url.clone()]),
        ))));
        let adapter = PkarrResolverAdapter::new(
            pkarr,
            Arc::new(|_addr| Vec::new()),
        );
        let result = adapter.resolve(&OwnerAddr([0u8; 32])).await;
        assert!(result.is_empty());
    }

    // Full end-to-end (publish to mock, resolve via adapter) is in
    // tests/pkarr_community_fallback_integration.rs (Task 9).
}
```

Add `pub mod pkarr_resolver_adapter;` to lib.rs.

- [ ] **Step 2: Gates + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked -p harmony-app pkarr_resolver_adapter 2>&1 | tail -10
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
git add src-tauri/src/pkarr_resolver_adapter.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-323): PkarrResolverAdapter — case C in-community fallback"
```

---

## Task 5: pkarr_invite_publisher — case A lifecycle

**Purpose:** Tracks active invites; on `generate_invite` registers `(invite_id, derived_key, expires_at)` with shared PkarrPublisher; unregisters on consumption/expiry/revoke.

**Files:**
- Create: `src-tauri/src/pkarr_invite_publisher.rs`
- Modify: `src-tauri/src/lib.rs` — `pub mod pkarr_invite_publisher;`

- [ ] **Step 1: Write the module**

Create `src-tauri/src/pkarr_invite_publisher.rs`:

```rust
//! Case A publisher — publishes alice's iroh routing under HKDF(invite_token.sig, epoch)
//! while an invite is pending. Stops publishing on consumption / expiry / revoke.
//!
//! Each pending invite gets its own derived key (different sig per invite),
//! so multiple concurrent invites coexist without DHT key collision.

use harmony_pkarr::{derive_ephemeral_key, current_epoch_id, PkarrCase, PkarrPublisher, PkarrRoutingRecord, RecordBuilder};
use std::sync::Arc;

use crate::community_invite::CommunityInvitePayload;

pub struct PkarrInvitePublisher {
    publisher: Arc<PkarrPublisher>,
    identity_signing_key: ed25519_dalek::SigningKey,
    identity_pub: [u8; 64],
    routing_blob_builder: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
}

impl PkarrInvitePublisher {
    pub fn new(
        publisher: Arc<PkarrPublisher>,
        identity_signing_key: ed25519_dalek::SigningKey,
        identity_pub: [u8; 64],
        routing_blob_builder: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
    ) -> Self {
        Self {
            publisher,
            identity_signing_key,
            identity_pub,
            routing_blob_builder,
        }
    }

    /// Called from the IPC layer after `generate_invite` succeeds.
    pub async fn register_invite(&self, invite: &CommunityInvitePayload) {
        let Some(token) = &invite.invite_token else {
            // Open community invites don't carry a token sig in the same way;
            // skip pkarr publish for now (Phase 3 may extend).
            return;
        };
        let epoch_id = current_epoch_id(now_ms());
        let signing = derive_ephemeral_key(PkarrCase::Invite, &token.sig, &epoch_id.to_be_bytes());

        let id_sk = self.identity_signing_key.clone();
        let id_pub = self.identity_pub;
        let blob_builder = Arc::clone(&self.routing_blob_builder);
        let builder: RecordBuilder = Arc::new(move |now_ms| {
            PkarrRoutingRecord::sign_new(blob_builder(), id_pub, now_ms, &id_sk)
                .expect("sign — fixed-size buffers should not fail")
        });

        let handle = format!("invite:{}", hex::encode(token.sig));
        self.publisher.register(handle, signing, builder).await;
    }

    /// Called when the invite is consumed, expires, or is revoked.
    pub async fn unregister_invite(&self, invite_token_sig: &[u8; 64]) {
        let handle = format!("invite:{}", hex::encode(invite_token_sig));
        self.publisher.unregister(&handle).await;
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time")
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use harmony_pkarr::{RelayClient, RelayPool, testing::MockPkarrRelay};
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    fn build_identity_pub(sk: &SigningKey) -> [u8; 64] {
        let mut out = [0u8; 64];
        out[32..].copy_from_slice(&sk.verifying_key().to_bytes());
        out
    }

    #[tokio::test]
    async fn register_then_unregister_does_not_panic() {
        let relay = MockPkarrRelay::start().await;
        let publisher = Arc::new(PkarrPublisher::new(Arc::new(RelayClient::new(
            RelayPool::new(vec![relay.base_url.clone()]),
        ))));
        let _ph = Arc::clone(&publisher).spawn();

        let sk = SigningKey::generate(&mut OsRng);
        let id_pub = build_identity_pub(&sk);
        let inv_pub = PkarrInvitePublisher::new(
            publisher,
            sk,
            id_pub,
            Arc::new(|| b"fake-iroh-routing".to_vec()),
        );

        // Build a minimal CommunityInvitePayload with a known token.sig (just for keying).
        // Implementer: use the Phase 1 / ZEB-217 test fixtures already present in the codebase.
        // Look for `fn fixture_invite()` or similar in src/ or tests/ — reuse it.
        // For the smoke test, just verify the unregister path is safe when nothing was registered:
        inv_pub.unregister_invite(&[0u8; 64]).await;
    }
}
```

Add `pub mod pkarr_invite_publisher;` to lib.rs.

- [ ] **Step 2: Gates + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked -p harmony-app pkarr_invite_publisher 2>&1 | tail -10
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
git add src-tauri/src/pkarr_invite_publisher.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-323): PkarrInvitePublisher — case A lifecycle"
```

---

## Task 6: pkarr_identity_publisher — case B lifecycle

**Purpose:** Single publication keyed on `owner_identity_pub`. Toggled by user via `connectivity_set_identity_discoverable` IPC. Persisted to `PkarrSettings`.

**Files:**
- Create: `src-tauri/src/pkarr_identity_publisher.rs`
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Write the module**

Create `src-tauri/src/pkarr_identity_publisher.rs`:

```rust
//! Case B publisher — publishes alice's iroh routing under HKDF(owner_pub, epoch)
//! when user opts in via "Make me discoverable" toggle. Persisted via PkarrSettings.

use harmony_pkarr::{derive_ephemeral_key, current_epoch_id, PkarrCase, PkarrPublisher, PkarrRoutingRecord, RecordBuilder};
use std::sync::Arc;

pub struct PkarrIdentityPublisher {
    publisher: Arc<PkarrPublisher>,
    identity_signing_key: ed25519_dalek::SigningKey,
    identity_pub: [u8; 64],
    routing_blob_builder: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
}

const HANDLE: &str = "identity";

impl PkarrIdentityPublisher {
    pub fn new(
        publisher: Arc<PkarrPublisher>,
        identity_signing_key: ed25519_dalek::SigningKey,
        identity_pub: [u8; 64],
        routing_blob_builder: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
    ) -> Self {
        Self {
            publisher,
            identity_signing_key,
            identity_pub,
            routing_blob_builder,
        }
    }

    pub async fn enable(&self) {
        let epoch_id = current_epoch_id(now_ms());
        let signing = derive_ephemeral_key(
            PkarrCase::Identity,
            &self.identity_pub,
            &epoch_id.to_be_bytes(),
        );

        let id_sk = self.identity_signing_key.clone();
        let id_pub = self.identity_pub;
        let blob_builder = Arc::clone(&self.routing_blob_builder);
        let builder: RecordBuilder = Arc::new(move |now_ms| {
            PkarrRoutingRecord::sign_new(blob_builder(), id_pub, now_ms, &id_sk).expect("sign")
        });

        self.publisher.register(HANDLE.to_string(), signing, builder).await;
    }

    pub async fn disable(&self) {
        self.publisher.unregister(HANDLE).await;
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time")
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use harmony_pkarr::{RelayClient, RelayPool, testing::MockPkarrRelay};
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    fn build_id_pub(sk: &SigningKey) -> [u8; 64] {
        let mut out = [0u8; 64];
        out[32..].copy_from_slice(&sk.verifying_key().to_bytes());
        out
    }

    #[tokio::test]
    async fn enable_then_disable_round_trip() {
        let relay = MockPkarrRelay::start().await;
        let publisher = Arc::new(PkarrPublisher::new(Arc::new(RelayClient::new(
            RelayPool::new(vec![relay.base_url.clone()]),
        ))));
        let _ph = Arc::clone(&publisher).spawn();

        let sk = SigningKey::generate(&mut OsRng);
        let id_pub = build_id_pub(&sk);
        let id_pub_publisher = PkarrIdentityPublisher::new(
            Arc::clone(&publisher),
            sk,
            id_pub,
            Arc::new(|| b"fake-routing".to_vec()),
        );

        id_pub_publisher.enable().await;
        assert!(publisher.active_handles().await.contains(&"identity".to_string()));
        id_pub_publisher.disable().await;
        assert!(!publisher.active_handles().await.contains(&"identity".to_string()));
    }
}
```

Add `pub mod pkarr_identity_publisher;` to lib.rs.

- [ ] **Step 2: Gates + commit**

```bash
cargo nextest run --locked -p harmony-app pkarr_identity_publisher 2>&1 | tail -5
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
git add src-tauri/src/pkarr_identity_publisher.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-323): PkarrIdentityPublisher — case B lifecycle"
```

---

## Task 7: pkarr_community_publisher — case C lifecycle

**Purpose:** Per-community publication keyed on `HKDF(EpochKey ‖ owner_pub, epoch)`. Lifecycle tied to community membership (create/join/leave/kick).

**Files:**
- Create: `src-tauri/src/pkarr_community_publisher.rs`
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Write the module**

Mirrors PkarrInvitePublisher's shape:

```rust
//! Case C publisher — publishes alice's iroh routing per community she's in,
//! keyed by HKDF(EpochKey ‖ own_identity_pub, epoch). Used by other community
//! members' resolvers when Phase 1's CRDT-broadcast routing is stale.

use harmony_pkarr::{derive_ephemeral_key, current_epoch_id, PkarrCase, PkarrPublisher, PkarrRoutingRecord, RecordBuilder};
use std::sync::Arc;

use crate::owner_state_types::SpaceId;

pub struct PkarrCommunityPublisher {
    publisher: Arc<PkarrPublisher>,
    identity_signing_key: ed25519_dalek::SigningKey,
    identity_pub: [u8; 64],
    routing_blob_builder: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
}

impl PkarrCommunityPublisher {
    pub fn new(
        publisher: Arc<PkarrPublisher>,
        identity_signing_key: ed25519_dalek::SigningKey,
        identity_pub: [u8; 64],
        routing_blob_builder: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
    ) -> Self {
        Self {
            publisher,
            identity_signing_key,
            identity_pub,
            routing_blob_builder,
        }
    }

    pub async fn on_community_joined(&self, community_id: SpaceId, epoch_key: [u8; 32]) {
        let epoch_id = current_epoch_id(now_ms());
        let mut info = Vec::with_capacity(64 + 8);
        info.extend_from_slice(&self.identity_pub);
        info.extend_from_slice(&epoch_id.to_be_bytes());
        let signing = derive_ephemeral_key(PkarrCase::Community, &epoch_key, &info);

        let id_sk = self.identity_signing_key.clone();
        let id_pub = self.identity_pub;
        let blob_builder = Arc::clone(&self.routing_blob_builder);
        let builder: RecordBuilder = Arc::new(move |now_ms| {
            PkarrRoutingRecord::sign_new(blob_builder(), id_pub, now_ms, &id_sk).expect("sign")
        });

        let handle = format!("community:{}", hex::encode(community_id.0));
        self.publisher.register(handle, signing, builder).await;
    }

    pub async fn on_community_left_or_kicked(&self, community_id: SpaceId) {
        let handle = format!("community:{}", hex::encode(community_id.0));
        self.publisher.unregister(&handle).await;
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time")
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use harmony_pkarr::{RelayClient, RelayPool, testing::MockPkarrRelay};
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    fn build_id_pub(sk: &SigningKey) -> [u8; 64] {
        let mut out = [0u8; 64];
        out[32..].copy_from_slice(&sk.verifying_key().to_bytes());
        out
    }

    #[tokio::test]
    async fn join_then_leave_round_trip() {
        let relay = MockPkarrRelay::start().await;
        let publisher = Arc::new(PkarrPublisher::new(Arc::new(RelayClient::new(
            RelayPool::new(vec![relay.base_url.clone()]),
        ))));
        let _ph = Arc::clone(&publisher).spawn();

        let sk = SigningKey::generate(&mut OsRng);
        let id_pub = build_id_pub(&sk);
        let com_pub = PkarrCommunityPublisher::new(
            Arc::clone(&publisher),
            sk,
            id_pub,
            Arc::new(|| b"routing".to_vec()),
        );

        let community_id = SpaceId([7u8; 32]);
        let epoch_key = [0xAAu8; 32];
        com_pub.on_community_joined(community_id, epoch_key).await;
        assert!(publisher.active_handles().await.iter().any(|h| h.starts_with("community:")));
        com_pub.on_community_left_or_kicked(community_id).await;
        assert!(!publisher.active_handles().await.iter().any(|h| h.starts_with("community:")));
    }
}
```

Add `pub mod pkarr_community_publisher;` to lib.rs.

- [ ] **Step 2: Gates + commit**

```bash
cargo nextest run --locked -p harmony-app pkarr_community_publisher 2>&1 | tail -5
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
git add src-tauri/src/pkarr_community_publisher.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-323): PkarrCommunityPublisher — case C lifecycle"
```

---

## Task 8: Tauri IPCs + events + lib.rs wiring

**Purpose:** 5 new IPCs + 3 new events + NodeState extension to hold the pkarr publisher/resolver and the case-specific policy modules; wire everything at boot.

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Extend NodeState**

Add to NodeState struct:
```rust
pub pkarr_publisher: Option<Arc<harmony_pkarr::PkarrPublisher>>,
pub pkarr_resolver: Option<Arc<harmony_pkarr::PkarrResolver>>,
pub pkarr_invite_publisher: Option<Arc<pkarr_invite_publisher::PkarrInvitePublisher>>,
pub pkarr_identity_publisher: Option<Arc<pkarr_identity_publisher::PkarrIdentityPublisher>>,
pub pkarr_community_publisher: Option<Arc<pkarr_community_publisher::PkarrCommunityPublisher>>,
pub pkarr_settings_path: Option<PathBuf>,
pub pkarr_publisher_handle: Option<tokio::task::JoinHandle<()>>,
```

- [ ] **Step 2: Boot wiring**

In the iroh boot block of `start_node` (the same place Phase 1's iroh_endpoint + ReachabilityResolver are wired), after the iroh endpoint is up:

```rust
// Phase 2 pkarr wiring (ZEB-323).
let pkarr_relays = vec![
    "https://relay.pkarr.org".to_string(), // n0-equivalent fallback
    "https://i.q8.fyi/pkarr".to_string(),  // jake's self-hosted (TBD: confirm endpoint)
];
let relay_pool = harmony_pkarr::RelayPool::new(pkarr_relays);
let relay_client = Arc::new(harmony_pkarr::RelayClient::new(relay_pool));
let pkarr_publisher = Arc::new(harmony_pkarr::PkarrPublisher::new(Arc::clone(&relay_client)));
let pkarr_publisher_handle = Arc::clone(&pkarr_publisher).spawn();
let pkarr_resolver = Arc::new(harmony_pkarr::PkarrResolver::new(Arc::clone(&relay_client)));

// Wire case C fallback into Phase 1's ReachabilityResolver.
let state_for_contexts = Arc::clone(&state);
let contexts_fn: Arc<dyn Fn(&OwnerAddr) -> Vec<pkarr_resolver_adapter::PkarrCommunityContext> + Send + Sync> =
    Arc::new(move |target_addr: &OwnerAddr| {
        // Walk every community in state and produce (community_id, EpochKey, target_identity_pub).
        // Implementer: read state.communities; for each, get EpochKey + look up the target's identity_pub
        // from the community's members map. Return empty vec for owners not in any shared community.
        let state = state_for_contexts.lock(); // adapt to actual lock type
        // ... iterate state.communities, filter, build contexts ...
        Vec::new() // stub — implementer fills in
    });

let adapter = Arc::new(pkarr_resolver_adapter::PkarrResolverAdapter::new(
    Arc::clone(&pkarr_resolver),
    contexts_fn,
));
reachability_resolver.set_fallback_source(adapter);

// Build the three case policies.
let blob_builder: Arc<dyn Fn() -> Vec<u8> + Send + Sync> = {
    let resolver = Arc::clone(&reachability_resolver);
    Arc::new(move || {
        // Encode the local device's own ReachabilityRecord via the existing
        // canonical CBOR encoder.
        let payload = build_local_reachability_payload(); // existing helper
        let mut out = Vec::new();
        ciborium::into_writer(&payload, &mut out).expect("encode");
        out
    })
};
let pkarr_invite_pub = Arc::new(pkarr_invite_publisher::PkarrInvitePublisher::new(
    Arc::clone(&pkarr_publisher),
    identity_signing_key.clone(),
    identity_pub_bytes,
    Arc::clone(&blob_builder),
));
let pkarr_identity_pub = Arc::new(pkarr_identity_publisher::PkarrIdentityPublisher::new(
    Arc::clone(&pkarr_publisher),
    identity_signing_key.clone(),
    identity_pub_bytes,
    Arc::clone(&blob_builder),
));
let pkarr_community_pub = Arc::new(pkarr_community_publisher::PkarrCommunityPublisher::new(
    Arc::clone(&pkarr_publisher),
    identity_signing_key.clone(),
    identity_pub_bytes,
    Arc::clone(&blob_builder),
));

// Load case-B setting; if discoverable, enable.
let settings_path = app_handle.path().app_data_dir().expect("data dir").join("connectivity-settings.json");
let settings = pkarr_settings::PkarrSettings::load_or_default(&settings_path);
if settings.identity_discoverable {
    pkarr_identity_pub.enable().await;
}

// Bootstrap case-C: enable per-community publication for every existing community.
for (community_id, community_state) in state.lock().communities.iter() {
    let epoch_key = community_state.current_epoch_key(); // adapt to actual API
    pkarr_community_pub.on_community_joined(*community_id, epoch_key).await;
}

// Stash on NodeState.
let mut guard = state.lock();
guard.pkarr_publisher = Some(pkarr_publisher);
guard.pkarr_resolver = Some(pkarr_resolver);
guard.pkarr_invite_publisher = Some(pkarr_invite_pub);
guard.pkarr_identity_publisher = Some(pkarr_identity_pub);
guard.pkarr_community_publisher = Some(pkarr_community_pub);
guard.pkarr_settings_path = Some(settings_path);
guard.pkarr_publisher_handle = Some(pkarr_publisher_handle);
```

- [ ] **Step 3: Wire case-A trigger into generate_invite**

Find the existing `generate_invite` IPC handler in lib.rs. After the invite payload is generated, call:
```rust
if let Some(inv_pub) = state.lock().pkarr_invite_publisher.clone() {
    inv_pub.register_invite(&payload).await;
}
```

And in the event handler for community-state-changed (or wherever the joiner becomes a member), unregister:
```rust
if let Some(inv_pub) = state.lock().pkarr_invite_publisher.clone() {
    inv_pub.unregister_invite(&consumed_invite_token_sig).await;
}
```

- [ ] **Step 4: Wire case-C trigger into community-create/leave/kick**

In whatever lib.rs path handles community create/join/leave/kick events, mirror the boot-time enumeration call to `on_community_joined` / `on_community_left_or_kicked`.

- [ ] **Step 5: Implement the 5 new IPCs**

Add to lib.rs (with `#[tauri::command(rename_all = "snake_case")]`):

```rust
#[tauri::command(rename_all = "snake_case")]
async fn connectivity_redeem_invite_iroh(
    state: tauri::State<'_, Arc<Mutex<NodeState>>>,
    invite_url: String,
) -> Result<RedemptionOutcome, String> {
    // 1. Decode URL → CommunityInvitePayload (existing community_invite::decode_url helper)
    // 2. Verify outer payload sigs (existing logic)
    // 3. Derive case-A key from invite_token.sig + current_epoch_id(now_ms())
    // 4. PkarrResolver.resolve_window with [epoch-1, epoch, epoch+1] keys
    // 5. Verify inner sig binds to admin_identity_pub
    // 6. Parse routing_blob → iroh NodeId + relay + addrs
    // 7. Iroh::Endpoint::connect, open zenoh-over-iroh session
    // 8. Send CommunityInviteSigned via zenoh
    // 9. Await counter-signed response
    // 10. Insert into CRDT
    // On any step failure: fall back to existing Reticulum redeem path (call existing IPC's
    //    inner helper if available; else return InviterUnreachable).
    todo!("orchestration — see plan Section 7.2 + existing community_invite::handle_unicast for pattern")
}

#[tauri::command(rename_all = "snake_case")]
async fn connectivity_set_identity_discoverable(
    state: tauri::State<'_, Arc<Mutex<NodeState>>>,
    enabled: bool,
) -> Result<(), String> {
    let (id_pub, settings_path) = {
        let guard = state.lock();
        (
            guard.pkarr_identity_publisher.clone(),
            guard.pkarr_settings_path.clone(),
        )
    };
    let (Some(id_pub), Some(path)) = (id_pub, settings_path) else {
        return Err("pkarr publisher not initialized".into());
    };

    let mut settings = pkarr_settings::PkarrSettings::load_or_default(&path);
    settings.identity_discoverable = enabled;
    settings.save(&path).map_err(|e| format!("save: {e}"))?;

    if enabled {
        id_pub.enable().await;
    } else {
        id_pub.disable().await;
    }

    // Emit event
    app_handle.emit("connectivity-identity-discoverable-changed", serde_json::json!({"enabled": enabled}))
        .map_err(|e| format!("emit: {e}"))?;

    Ok(())
}

#[tauri::command(rename_all = "snake_case")]
async fn connectivity_get_identity_discoverable(
    state: tauri::State<'_, Arc<Mutex<NodeState>>>,
) -> Result<bool, String> {
    let path = state.lock().pkarr_settings_path.clone()
        .ok_or("pkarr settings not initialized")?;
    Ok(pkarr_settings::PkarrSettings::load_or_default(&path).identity_discoverable)
}

#[tauri::command(rename_all = "snake_case")]
async fn connectivity_discover_identity(
    state: tauri::State<'_, Arc<Mutex<NodeState>>>,
    identity_pub_hex: String,
) -> Result<Option<DiscoveredRecord>, String> {
    let identity_pub: [u8; 64] = hex::decode(&identity_pub_hex)
        .map_err(|e| format!("hex decode: {e}"))?
        .try_into()
        .map_err(|_| "identity_pub must be 64 bytes")?;

    let resolver = state.lock().pkarr_resolver.clone()
        .ok_or("pkarr resolver not initialized")?;

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("clock: {e}"))?
        .as_millis() as u64;
    let window = harmony_pkarr::epoch_tolerance_window(now_ms);
    let keys: Vec<_> = window.iter().map(|&e| {
        let signing = harmony_pkarr::derive_ephemeral_key(
            harmony_pkarr::PkarrCase::Identity,
            &identity_pub,
            &e.to_be_bytes(),
        );
        signing.verifying_key()
    }).collect();

    let Some(rec) = resolver.resolve_window(&keys).await.map_err(|e| format!("resolve: {e}"))? else {
        return Ok(None);
    };
    rec.verify_inner_sig().map_err(|_| "inner sig invalid")?;
    rec.verify_identity_match(&identity_pub).map_err(|_| "identity mismatch")?;
    rec.verify_skew(now_ms).map_err(|_| "skew")?;

    // Decode routing_blob into DiscoveredRecord (mirrors ReachabilityAnnouncePayload shape).
    let payload: ReachabilityAnnouncePayload = ciborium::from_reader(rec.routing_blob.as_slice())
        .map_err(|e| format!("decode: {e}"))?;
    Ok(Some(DiscoveredRecord::from(payload)))
}

#[tauri::command(rename_all = "snake_case")]
async fn connectivity_pkarr_publication_status(
    state: tauri::State<'_, Arc<Mutex<NodeState>>>,
) -> Result<PublicationStatus, String> {
    let publisher = state.lock().pkarr_publisher.clone()
        .ok_or("pkarr publisher not initialized")?;
    let handles = publisher.active_handles().await;
    Ok(PublicationStatus {
        invite_count: handles.iter().filter(|h| h.starts_with("invite:")).count(),
        identity_active: handles.iter().any(|h| *h == "identity"),
        community_count: handles.iter().filter(|h| h.starts_with("community:")).count(),
    })
}
```

Define helper types:
```rust
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RedemptionOutcome {
    pub status: String, // "joined" | "inviter_unreachable" | "fallback_reticulum" | etc.
    pub community_id: Option<String>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredRecord {
    pub iroh_node_id: String,
    pub relay_url: Option<String>,
    pub direct_addrs: Vec<String>,
    pub announced_at_ms: u64,
}

impl From<ReachabilityAnnouncePayload> for DiscoveredRecord {
    fn from(p: ReachabilityAnnouncePayload) -> Self {
        Self {
            iroh_node_id: hex::encode(p.iroh_node_id),
            relay_url: p.home_relay_url,
            direct_addrs: p.direct_addresses.iter().map(|a| a.to_string()).collect(),
            announced_at_ms: p.announced_at_ms,
        }
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicationStatus {
    pub invite_count: usize,
    pub identity_active: bool,
    pub community_count: usize,
}
```

Register the 5 new IPCs in the Tauri builder's `invoke_handler` macro list (alongside Phase 1's).

- [ ] **Step 6: Gates + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
cargo nextest run --locked -p harmony-app --lib 2>&1 | tail -10
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-323): 5 new connectivity_* Tauri IPCs + 3 events + pkarr boot wiring"
```

(Implementer note: the lib.rs change is large. If a chunk of this task — e.g., the case-C contexts_fn closure — proves harder than expected because of Phase 1's NodeState locking pattern being non-obvious, mark DONE_WITH_CONCERNS and report exactly what needs the controller's help.)

---

## Task 9: Integration tests + wire-format pin

**Purpose:** Three end-to-end integration tests + a wire-format pin file for the on-the-wire `PkarrRoutingRecord` as serialized by harmony-client's routing blob encoder.

**Files:**
- Create: `src-tauri/tests/pkarr_invite_redemption_integration.rs`
- Create: `src-tauri/tests/pkarr_identity_discovery_integration.rs`
- Create: `src-tauri/tests/pkarr_community_fallback_integration.rs`
- Create: `src-tauri/tests/wire_format_pkarr_routing_record_fixtures.rs` (NOTE: harmony-pkarr already has one for the inner record; this one pins the harmony-client routing_blob encoding)

- [ ] **Step 1: Write the wire-format pin test**

`tests/wire_format_pkarr_routing_record_fixtures.rs`:
```rust
//! Pins the on-the-wire bytes of harmony-client's routing_blob (the opaque
//! payload embedded in a PkarrRoutingRecord).

use harmony_app::reachability_record::ReachabilityAnnouncePayload;

#[test]
fn routing_blob_canonical_cbor_pinned() {
    // Implementer: build a deterministic ReachabilityAnnouncePayload using
    // fixed inputs (matching Phase 1's wire_format_reachability_announce_fixtures
    // pattern). Encode via ciborium::into_writer. Pin the resulting hex.
    todo!("write the test using Phase 1's pattern as a template")
}
```

Then capture + pin the hex via temporary eprintln, same pattern as harmony-pkarr's pin test.

- [ ] **Step 2: Write the case A integration test**

`tests/pkarr_invite_redemption_integration.rs`:
- Start `MockPkarrRelay` (from harmony_pkarr::testing).
- Build a CommunityInvitePayload with deterministic invite_token.sig.
- Spin up "alice's" pkarr_invite_publisher pointing at the mock relay.
- Wait for publication.
- "Bob's" side: derive the same key from token.sig + epoch; query the mock relay via `PkarrResolver`; verify the record decodes; verify inner sig binds to alice's identity_pub.
- (Full iroh QUIC round-trip is heavier — defer to a Phase 3 manual e2e test on real hardware. Phase 2 unit-level integration is sufficient.)
- Wall-clock budget: 30s timeout.

- [ ] **Step 3: Write the case B integration test**

Similar shape to case A but using PkarrIdentityPublisher / `connectivity_discover_identity` semantics.

- [ ] **Step 4: Write the case C integration test**

- Build a stub Phase 1 ReachabilityResolver (empty map).
- Wire PkarrResolverAdapter as fallback with a custom contexts_fn that returns one community context (deterministic EpochKey + target_identity_pub).
- Have a separate PkarrCommunityPublisher (deterministic key derivation) publish to the mock relay.
- Call `resolver.resolve_async(addr).await`; assert the returned payload matches what was published.
- Assert that a subsequent sync `resolve(addr)` returns the same record (warm-cache check).

- [ ] **Step 5: Gates + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures --test pkarr_invite_redemption_integration 2>&1 | tail -10
cargo nextest run --locked --features test-fixtures --test pkarr_identity_discovery_integration 2>&1 | tail -10
cargo nextest run --locked --features test-fixtures --test pkarr_community_fallback_integration 2>&1 | tail -10
cargo nextest run --locked --features test-fixtures --test wire_format_pkarr_routing_record_fixtures 2>&1 | tail -5
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
git add src-tauri/tests/pkarr_invite_redemption_integration.rs src-tauri/tests/pkarr_identity_discovery_integration.rs src-tauri/tests/pkarr_community_fallback_integration.rs src-tauri/tests/wire_format_pkarr_routing_record_fixtures.rs
git commit -m "test(zeb-323): integration tests for cases A/B/C + wire-format pin"
```

---

## Task 10: Frontend — types + adapter + 3 UX deltas

**Purpose:** TS types for the new IPCs + Svelte components for the join flow extension + Settings toggle + Diagnostics panel addition.

**Files:**
- Modify: `src/lib/types/connectivity.ts` — add `DiscoveredRecord`, `PublicationStatus`, `RedemptionOutcome`
- Modify: `src/lib/connectivity-adapter.ts` — wrap 5 new IPCs + 3 new event listeners
- Modify: `src/lib/components/RedeemInviteDialog.svelte`
- Modify (or create): `src/lib/components/Settings.svelte` (or wherever settings live)
- Modify: `src/lib/components/DiagnosticsPanel.svelte`

- [ ] **Step 1: Extend types**

In `src/lib/types/connectivity.ts`, add:
```typescript
export interface DiscoveredRecord {
  irohNodeId: string;
  relayUrl?: string;
  directAddrs: string[];
  announcedAtMs: number;
}

export interface PublicationStatus {
  inviteCount: number;
  identityActive: boolean;
  communityCount: number;
}

export interface RedemptionOutcome {
  status: 'joined' | 'inviter_unreachable' | 'fallback_reticulum' | string;
  communityId?: string;
}

export type RedemptionStage =
  | 'resolving'
  | 'connecting'
  | 'sending'
  | 'awaiting_countersig'
  | 'joined';

export interface ResolutionProgressEvent {
  inviteId: string;
  stage: RedemptionStage;
  attemptN: number;
}
```

- [ ] **Step 2: Extend the adapter**

In `src/lib/connectivity-adapter.ts`, add wrappers (camelCase JS ↔ snake_case Rust):

```typescript
export async function redeemInviteIroh(inviteUrl: string): Promise<RedemptionOutcome> {
  try {
    return await invoke<RedemptionOutcome>('connectivity_redeem_invite_iroh', { inviteUrl });
  } catch (e) {
    throw new Error(e instanceof Error ? e.message : String(e));
  }
}

export async function setIdentityDiscoverable(enabled: boolean): Promise<void> {
  try {
    await invoke('connectivity_set_identity_discoverable', { enabled });
  } catch (e) {
    throw new Error(e instanceof Error ? e.message : String(e));
  }
}

export async function getIdentityDiscoverable(): Promise<boolean> {
  try {
    return await invoke<boolean>('connectivity_get_identity_discoverable');
  } catch (e) {
    throw new Error(e instanceof Error ? e.message : String(e));
  }
}

export async function discoverIdentity(identityPubHex: string): Promise<DiscoveredRecord | null> {
  try {
    return await invoke<DiscoveredRecord | null>('connectivity_discover_identity', { identityPubHex });
  } catch (e) {
    throw new Error(e instanceof Error ? e.message : String(e));
  }
}

export async function pkarrPublicationStatus(): Promise<PublicationStatus> {
  try {
    return await invoke<PublicationStatus>('connectivity_pkarr_publication_status');
  } catch (e) {
    throw new Error(e instanceof Error ? e.message : String(e));
  }
}

// Event subscribers (use the same pattern as Phase 1's onReachabilityChanged):
export function onResolutionProgress(cb: (ev: ResolutionProgressEvent) => void): () => void {
  let unlisten: UnlistenFn | undefined;
  let destroyed = false;
  listen<ResolutionProgressEvent>('connectivity-invite-resolution-progress', e => cb(e.payload))
    .then(u => { if (destroyed) u(); else unlisten = u; })
    .catch(e => console.error('listen failed:', e instanceof Error ? e.message : String(e)));
  return () => { destroyed = true; unlisten?.(); };
}

export function onIdentityDiscoverableChanged(cb: (enabled: boolean) => void): () => void {
  // Same shape — extract enabled from payload.
  // (implementer: mirror onResolutionProgress)
}

export function onPkarrFallbackFired(cb: (ev: { peerAddrShort: string; communityId: string; hit: boolean }) => void): () => void {
  // Same shape.
}
```

- [ ] **Step 3: Extend RedeemInviteDialog.svelte**

Wire the new `redeemInviteIroh` IPC. Display progress stages via `onResolutionProgress`. On `inviter_unreachable`, show "Couldn't reach the inviter through the network right now. They may be offline; try again later."

- [ ] **Step 4: Add Settings Privacy toggle**

Find the existing Settings panel (search: `grep -rln "Settings" src/lib/components/`). Add a new "Network Discoverability" section:
- Toggle bound to `getIdentityDiscoverable()` / `setIdentityDiscoverable(...)`.
- Default OFF (matches backend default).
- Helper text: "When on, anyone who has your identity address can connect to your devices over the internet. When off, you can only be reached through invite links and communities you already share."

- [ ] **Step 5: Extend DiagnosticsPanel.svelte**

Add new collapsible section "Network Discovery (pkarr)" displaying:
- # active publications by case (A/B/C counts from `pkarrPublicationStatus()`).
- Subscribe to `onPkarrFallbackFired` and display the last 5 fallback events.

- [ ] **Step 6: Vitest tests**

For each modified component, add a vitest test in the same file's `__tests__/` dir (mirroring Phase 1's pattern with `RedeemInviteDialog.test.ts`, `DiagnosticsPanel.test.ts`):
- Mock the new IPC adapter functions (vi.mock).
- Render the component, verify UI reflects state.
- Trigger user interactions, verify the adapter is called with correct args.

- [ ] **Step 7: Gates + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit 2>&1 | tail -5
npx vitest run 2>&1 | tail -10
git add src/
git commit -m "feat(zeb-323): frontend — types + adapter + RedeemInviteDialog + Settings privacy + Diagnostics"
```

---

## Task 11: Final sweep + pin to merge SHA + push branch + open PR

**Purpose:** Final 5-gate sweep. If harmony PR #270 has merged by now, update `harmony-pkarr` pin from branch ref to merge commit SHA. Push branch + create PR.

- [ ] **Step 1: Check PR #270 status**

```bash
gh -R zeblithic/harmony pr view 270 --json state,mergedAt,mergeCommit 2>&1 | tail -10
```

If `state: MERGED`: update `src-tauri/Cargo.toml` `harmony-pkarr` pin from `branch = "zeb-322-harmony-pkarr-crate"` to `rev = "<mergeCommit.oid>"`. Run `cargo update -p harmony-pkarr` to lockfile-pin.

If still `state: OPEN`: leave the branch pin in place; note in PR body that final pin will happen when PR #270 merges.

- [ ] **Step 2: Full gate sweep**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -30
```

Bash tool `timeout: 900000` (15 min) for nextest. Confirm pass count ≥ baseline + new tests. Confirm no NEW failures beyond `/tmp/zeb-323-baseline-failures.txt`.

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit
npx vitest run
```

Both pass.

- [ ] **Step 3: Confirm branch up to date**

```bash
git fetch origin --quiet
git log --oneline HEAD..origin/main | head -5
```

If non-empty, rebase: `git rebase origin/main`.

- [ ] **Step 4: Push branch**

```bash
git push -u origin zeb-323-harmony-client-pkarr-phase-2b
```

- [ ] **Step 5: Create PR**

CRITICAL — per `feedback_linear_pr_auto_close`: NEVER mention the bare string `ZEB-321` in the PR title or body. Use prose ("the cross-WAN connectivity initiative") if you need to reference the umbrella. Only `ZEB-323` (this PR's sub-ticket) goes in as a close-trigger.

```bash
gh pr create --title "feat(zeb-323): harmony-client pkarr policies + IPCs + UX (Phase 2b)" --body "$(cat <<'EOF'
## Summary

Wires the new harmony-pkarr primitive (shipped in companion harmony PR #270) into harmony-client with three case-specific policies:

- **Case A — invite-redemption**: alice's pending invites publish her current iroh routing to Mainline DHT under HKDF(invite_token.sig, epoch); bob's `connectivity_redeem_invite_iroh` IPC resolves it cross-WAN. Falls back to existing Reticulum path on pkarr failure.
- **Case B — opt-in identity-keyed**: per-device "Make me discoverable" toggle. When on, publishes under HKDF(owner_identity_pub, epoch). Anyone with my identity hash can find my current iroh routing.
- **Case C — in-community reconnection fallback**: Phase 1's `ReachabilityResolver` gets a new async `resolve_async()` that falls back to pkarr (HKDF(EpochKey ‖ owner_pub, epoch)) when in-memory CRDT map has no fresh entry. Only members of a shared community can resolve.

### Changes

- 5 new policy modules in src-tauri/src/ (pkarr_settings, pkarr_resolver_adapter, pkarr_invite_publisher, pkarr_identity_publisher, pkarr_community_publisher)
- Phase 1 surgical change: `ReachabilityResolver` gets `fallback_source` field + `resolve_async()`. Existing sync `resolve()` unchanged.
- 5 new Tauri IPCs (`connectivity_redeem_invite_iroh`, `connectivity_set_identity_discoverable`, `connectivity_get_identity_discoverable`, `connectivity_discover_identity`, `connectivity_pkarr_publication_status`)
- 3 new events (`connectivity-invite-resolution-progress`, `connectivity-identity-discoverable-changed`, `connectivity-pkarr-fallback-fired`)
- 3 UX changes (`RedeemInviteDialog.svelte` extended; Settings → Privacy → "Network Discoverability" toggle; `DiagnosticsPanel.svelte` extended)
- 3 backend integration tests + 1 wire-format pin
- ~2000 LOC of Rust + TS + Svelte

## Design

Full design: `docs/specs/2026-05-23-zeb-321-phase2-discovery-bootstrap-design.md` (commit cb5cca5). Closes [ZEB-323](https://linear.app/zeblith/issue/ZEB-323).

## Cross-repo coordination

Depends on harmony PR #270 (the harmony-pkarr crate). harmony-pkarr is pinned via git ref in `src-tauri/Cargo.toml`. **Merge PR #270 first.**

## Test plan

- [ ] `cargo fmt --all -- --check` clean
- [ ] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` clean
- [ ] `cargo nextest run --locked --workspace --all-targets --features test-fixtures` green (no regressions vs Task 0 baseline)
- [ ] `npx tsc --noEmit` clean
- [ ] `npx vitest run` green
- [ ] Manual smoke: settings toggle persists across app restart; diagnostics panel shows expected publication counts

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 6: Report PR URL + number**

```bash
gh pr view --json url,number,title | tail -5
```

---

## End of Plan

11 tasks. Estimated ~2000-2500 LOC across backend + frontend. PR 2 opens with the harmony-pkarr branch pin; final pin → merge SHA happens in Task 11 once PR #270 merges (or in a fixup commit after).

Downstream of this PR: both PRs go through bot-review convergence loop in parallel, with merge ordering preserved (PR #270 → PR #2). Pushover Jake when both reach mergeable + CLEAN.
