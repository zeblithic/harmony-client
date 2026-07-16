# ZEB-694 Introduction-Broker Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restructure the introduction rate limiter so quotas key on authenticated identities and don't collide across protocol roles, and make an AskMe introduction accept survive a failed dial instead of burning the staged offer.

**Architecture:** Two node-local, independent groups. Group A splits `IntroRateLimiter` (in `friend_intro.rs`) into two reusable primitives (`KeyedSlidingWindow<K>`, `KeyedDedupe<K>`) behind a two-tier container: a pre-auth flood shield keyed on the connection's iroh-authenticated `remote_id()`, plus post-auth per-role quotas keyed on the verified owner; the acceptor call sites move the owner-keyed admits to *after* authentication. Group B reorders the accept path from consume-then-dial to peek → dial → consume-only-on-`Linked`, behind two `NodeState`-free seams, with an RAII in-flight guard and a 7-day TTL on staged offers. No wire format changes.

**Tech Stack:** Rust (tokio, iroh QUIC transport, `std::collections::{HashMap, VecDeque, HashSet}`), Svelte 5 frontend, Tauri IPC, `cargo-nextest`, `vitest`.

## Global Constraints

- **No wire-format change.** `zeb375_pex_fixtures` and `zeb376_intro_fixtures` MUST stay byte-identical; any diff to `src-tauri/tests/wire_format/zeb37{5,6}_*` is a regression.
- **Benign-ack on shed, no oracle.** Every rate-limiter rejection (any tier) funnels to the existing `self.write_ack(&mut send).await` path; no new error variant reaches the wire. The `&'static str` reasons are for `tracing` only.
- **Fail-safe ordering preserved.** Pre-auth `admit_connection` runs strictly before `authenticate_introduce_request` / `verify_introduction`; post-auth `admit_requester`/`admit_voucher` run strictly after they succeed.
- **Memory bounded.** Every keyed map retains the 8192-cap two-pass eviction (stale-prune → `select_nth_unstable` oldest-evict to a 3/4 low-watermark). Constants `MAX_WINDOW_KEYS = MAX_DEDUPE_ENTRIES = 8192`.
- **`Linked`-gated durability unchanged.** `complete_introduction`'s internal `notify_dirty` + Case-D reconcile + `friend-list-changed` emit stay exactly where they are; Group B only changes *when the offer is consumed* relative to the returned outcome.
- **Iterative gates avoid the relink trap.** Per-task Rust gates use `--lib`-scoped clippy + nextest (a lib change under `--all-targets` relinks ~97 integration binaries, ~50 min). The final whole-branch sweep — and only it — runs `--workspace --all-targets`. Frontend gates: `npx tsc --noEmit` + `npx vitest run`.
- **Gate discipline:** commit BEFORE gating; run cargo FOREGROUND (single blocking call). MSRV/fmt/clippy `-D warnings`.

---

## File Structure

| File | Responsibility in this plan |
|---|---|
| `src-tauri/src/friend_intro.rs` | Rate-limiter primitives + two-tier container. Tasks A1, A2, A3. |
| `src-tauri/src/iroh_pex_acceptor.rs` | Both `serve` arms call the new tiered methods at the correct auth boundary. Task A3 (atomic with the limiter restructure). |
| `src-tauri/src/friend_requests.rs` | `peek_offer`; in-flight guard (`accepting` set + `AcceptInFlightGuard`); TTL (`INTRODUCTION_OFFER_TTL_MS`, `is_offer_expired`, `sweep_expired_offers`). Tasks B1, B2, B3. |
| `src-tauri/src/lib.rs` | Accept-branch seams (`begin_introduction_accept`, `finalize_introduction_accept`) + reworked branch; `now_ms` threaded through `list_pending_friend_requests_inner`. Tasks B3, B4. |
| `src/lib/friend-service.ts`, `src/lib/components/FriendsPanel.svelte` | Keep the request row on a non-linked accept; surface the backend message. Task B5. |

**Current constants in `friend_intro.rs` (verbatim, reused):**
```rust
pub const INTRO_PER_VOUCHER_WINDOW_MS: u64 = 60 * 60 * 1000; // 1h
pub const INTRO_PER_VOUCHER_MAX: usize = 20;
pub const INTRO_DEDUPE_TTL_MS: u64 = 5 * 60 * 1000; // 5 min
const MAX_DEDUPE_ENTRIES: usize = 8192;
const MAX_WINDOW_KEYS: usize = 8192;
```

---

## Group A — Rate limiter

### Task A1: `KeyedSlidingWindow<K>` primitive

**Files:**
- Modify: `src-tauri/src/friend_intro.rs` (add near the existing `IntroRateLimiter`, before it)
- Test: inline `#[cfg(test)]` in `src-tauri/src/friend_intro.rs`

**Interfaces:**
- Consumes: module consts `MAX_WINDOW_KEYS`.
- Produces: `struct KeyedSlidingWindow<K>` with `fn new(max: usize, window_ms: u64) -> Self`, `fn admit(&mut self, key: K, now_ms: u64) -> bool` (true = admitted, false = over cap), `fn evict(&mut self, now_ms: u64)`. Bound: `K: Copy + Eq + std::hash::Hash`.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` in `friend_intro.rs` (create the module if the limiter tests aren't already there):

```rust
#[test]
fn keyed_window_enforces_cap_within_window() {
    let mut w = KeyedSlidingWindow::new(2, 1000);
    assert!(w.admit(7u64, 0));
    assert!(w.admit(7u64, 10));
    assert!(!w.admit(7u64, 20), "third within the window is over the cap");
    // a different key has its own budget
    assert!(w.admit(9u64, 20));
}

#[test]
fn keyed_window_prunes_stale_timestamps() {
    let mut w = KeyedSlidingWindow::new(1, 1000);
    assert!(w.admit(7u64, 0));
    assert!(!w.admit(7u64, 500), "still within the 1000ms window");
    assert!(w.admit(7u64, 1001), "t=0 pruned (cutoff=1), window empty again");
}

#[test]
fn keyed_window_evicts_over_cap_keys() {
    let mut w = KeyedSlidingWindow::new(1, 1_000_000);
    for k in 0u64..(MAX_WINDOW_KEYS as u64 + 100) {
        w.admit(k, k); // distinct keys, distinct timestamps
    }
    w.evict(MAX_WINDOW_KEYS as u64 + 100);
    assert!(w.windows.len() <= MAX_WINDOW_KEYS, "map bounded after eviction");
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(keyed_window)'`
Expected: FAIL — `cannot find type KeyedSlidingWindow`.

- [ ] **Step 3: Implement the primitive**

Add to `friend_intro.rs` (generalizes the window half + `evict_windows` of the current `admit`):

```rust
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

/// A per-key sliding-window counter, bounded to `MAX_WINDOW_KEYS` distinct keys.
/// Extracted from the ZEB-376 `IntroRateLimiter` so both the pre-auth connection
/// shield and the post-auth owner quotas share one audited implementation.
struct KeyedSlidingWindow<K> {
    max: usize,
    window_ms: u64,
    windows: HashMap<K, VecDeque<u64>>,
}

impl<K: Copy + Eq + Hash> KeyedSlidingWindow<K> {
    fn new(max: usize, window_ms: u64) -> Self {
        Self { max, window_ms, windows: HashMap::new() }
    }

    /// `true` if admitted (recorded), `false` if the key is at its in-window cap.
    fn admit(&mut self, key: K, now_ms: u64) -> bool {
        {
            let window = self.windows.entry(key).or_default();
            let cutoff = now_ms.saturating_sub(self.window_ms);
            while window.front().is_some_and(|&t| t < cutoff) {
                window.pop_front();
            }
            if window.len() >= self.max {
                return false;
            }
            window.push_back(now_ms);
        }
        self.evict(now_ms);
        true
    }

    fn evict(&mut self, now_ms: u64) {
        if self.windows.len() <= MAX_WINDOW_KEYS {
            return;
        }
        let cutoff = now_ms.saturating_sub(self.window_ms);
        self.windows.retain(|_, dq| {
            while dq.front().is_some_and(|&t| t < cutoff) {
                dq.pop_front();
            }
            !dq.is_empty()
        });
        if self.windows.len() <= MAX_WINDOW_KEYS {
            return;
        }
        let target = MAX_WINDOW_KEYS / 4 * 3;
        let mut recents: Vec<(u64, K)> = self
            .windows
            .iter()
            .map(|(&k, dq)| (*dq.back().expect("deque is non-empty after the stale prune"), k))
            .collect();
        let excess = recents.len() - target;
        recents.select_nth_unstable_by_key(excess, |&(ts, _)| ts);
        for &(_, k) in &recents[..excess] {
            self.windows.remove(&k);
        }
    }
}
```

(If `use std::collections::{HashMap, VecDeque};` / `use std::hash::Hash;` already exist at the top of the file, do not duplicate — reuse them.)

- [ ] **Step 4: Run to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(keyed_window)'`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/friend_intro.rs
git commit -m "feat(zeb-694): extract KeyedSlidingWindow<K> rate-limit primitive"
```

---

### Task A2: `KeyedDedupe<K>` primitive

**Files:**
- Modify: `src-tauri/src/friend_intro.rs`
- Test: inline `#[cfg(test)]` in `src-tauri/src/friend_intro.rs`

**Interfaces:**
- Consumes: module const `MAX_DEDUPE_ENTRIES`, `INTRO_DEDUPE_TTL_MS` (via caller-passed `ttl_ms`).
- Produces: `struct KeyedDedupe<K>` with `fn new(ttl_ms: u64) -> Self`, `fn is_duplicate(&self, key: K, now_ms: u64) -> bool`, `fn record(&mut self, key: K, now_ms: u64)`. Bound `K: Copy + Eq + Hash`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn keyed_dedupe_flags_repeat_within_ttl() {
    let mut d = KeyedDedupe::new(1000);
    assert!(!d.is_duplicate(5u64, 0), "never-seen key is not a duplicate");
    d.record(5u64, 0);
    assert!(d.is_duplicate(5u64, 500), "repeat within ttl is a duplicate");
    assert!(!d.is_duplicate(5u64, 1000), "past ttl is not a duplicate");
    assert!(!d.is_duplicate(6u64, 500), "different key is not a duplicate");
}

#[test]
fn keyed_dedupe_evicts_over_cap() {
    let mut d = KeyedDedupe::new(1_000_000);
    for k in 0u64..(MAX_DEDUPE_ENTRIES as u64 + 100) {
        d.record(k, k);
    }
    assert!(d.last_seen.len() <= MAX_DEDUPE_ENTRIES, "map bounded after record-time eviction");
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(keyed_dedupe)'`
Expected: FAIL — `cannot find type KeyedDedupe`.

- [ ] **Step 3: Implement the primitive**

Generalizes the `last_seen` half + `evict_last_seen`:

```rust
/// A per-key "last admitted at" map with a TTL duplicate check, bounded to
/// `MAX_DEDUPE_ENTRIES` distinct keys. Extracted from the ZEB-376 limiter.
struct KeyedDedupe<K> {
    ttl_ms: u64,
    last_seen: HashMap<K, u64>,
}

impl<K: Copy + Eq + Hash> KeyedDedupe<K> {
    fn new(ttl_ms: u64) -> Self {
        Self { ttl_ms, last_seen: HashMap::new() }
    }

    fn is_duplicate(&self, key: K, now_ms: u64) -> bool {
        self.last_seen
            .get(&key)
            .is_some_and(|&last| now_ms.saturating_sub(last) < self.ttl_ms)
    }

    fn record(&mut self, key: K, now_ms: u64) {
        self.last_seen.insert(key, now_ms);
        self.evict(now_ms);
    }

    fn evict(&mut self, now_ms: u64) {
        if self.last_seen.len() <= MAX_DEDUPE_ENTRIES {
            return;
        }
        self.last_seen
            .retain(|_, &mut ts| now_ms.saturating_sub(ts) < self.ttl_ms);
        if self.last_seen.len() <= MAX_DEDUPE_ENTRIES {
            return;
        }
        let target = MAX_DEDUPE_ENTRIES / 4 * 3;
        let mut stamps: Vec<(u64, K)> = self.last_seen.iter().map(|(&k, &ts)| (ts, k)).collect();
        let excess = stamps.len() - target;
        stamps.select_nth_unstable_by_key(excess, |&(ts, _)| ts);
        for &(_, k) in &stamps[..excess] {
            self.last_seen.remove(&k);
        }
    }
}
```

- [ ] **Step 4: Run to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(keyed_dedupe)'`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/friend_intro.rs
git commit -m "feat(zeb-694): extract KeyedDedupe<K> rate-limit primitive"
```

---

### Task A3: Restructure `IntroRateLimiter` + migrate Task-13 tests + rewire acceptor (ATOMIC)

**This task is compilation-atomic.** Removing the old `admit` method breaks its callers in the SAME compile unit — the two acceptor arms AND five existing ZEB-376 Task-13 unit tests. All three (restructure, test migration, acceptor rewire) MUST land in one commit or `cargo` won't build. Do not split them.

**Files:**
- Modify: `src-tauri/src/friend_intro.rs` — replace the `IntroRateLimiter` struct/`IntroRateLimiterInner`/`new`/`admit`/`evict_windows`/`evict_last_seen`/`tracked_len` (`:594-745` + the `tracked_len` helper) with the container below; add `INTRO_PER_CONNECTION_MAX`; migrate the five Task-13 tests (`:1284-1418`).
- Modify: `src-tauri/src/iroh_pex_acceptor.rs` — both `serve` arms (broker `:539-557`, voucher `:654-683`): `admit_connection` pre-auth + `admit_requester`/`admit_voucher` post-auth.
- Test: inline `#[cfg(test)]` in `src-tauri/src/friend_intro.rs`; regression via the 3-node e2e `src-tauri/tests/identity/introduction_broker_roundtrip_integration.rs`.

**Coverage note (no silent cap):** the shed/role-independence assertions live in the limiter unit tests below. `serve(&self, conn: &Connection)` has no unit harness for real iroh connections (the existing `iroh_pex_acceptor::tests` drive the pure decision fns, not `serve`), so a per-connection flood test at the acceptor would need a real two-endpoint integration harness of marginal value over the unit tests. The acceptor-level verification is therefore the existing 3-node e2e (happy path through both arms) staying green.

**Interfaces:**
- Consumes: `KeyedSlidingWindow<K>` (A1), `KeyedDedupe<K>` (A2), consts.
- Produces: `IntroRateLimiter` with `fn new() -> Self`, `fn with_caps(conn_max: usize, per_owner_max: usize, window_ms: u64, dedupe_ttl_ms: u64) -> Self`, and the three admit methods:
  - `pub fn admit_connection(&self, remote_id: [u8; 32], now_ms: u64) -> Result<(), &'static str>`
  - `pub fn admit_requester(&self, requester: OwnerAddr, target: OwnerAddr, now_ms: u64) -> Result<(), &'static str>`
  - `pub fn admit_voucher(&self, voucher: OwnerAddr, subject: OwnerAddr, now_ms: u64) -> Result<(), &'static str>`
  The old `pub fn admit(&self, key, subject, now_ms)` is REMOVED; this task updates its only callers (two acceptor arms + five Task-13 tests) in the same commit.
- Also consumes: `conn.remote_id()` (iroh authenticated endpoint id; `*conn.remote_id().as_bytes()` is `[u8; 32]`) — already in scope in `serve`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn limiter_roles_have_independent_budgets() {
    // Greptile regression: requester traffic must not starve an unrelated vouch.
    let rl = IntroRateLimiter::with_caps(100, 1, 3_600_000, 300_000);
    let o = OwnerAddr([1; 16]);
    let t = OwnerAddr([2; 16]);
    let t2 = OwnerAddr([3; 16]);
    let s = OwnerAddr([4; 16]);
    assert!(rl.admit_requester(o, t, 0).is_ok());
    assert_eq!(rl.admit_requester(o, t2, 1), Err("per-requester cap"), "requester at cap");
    assert!(rl.admit_voucher(o, s, 2).is_ok(), "voucher role has its own budget");
}

#[test]
fn limiter_connection_shield_sheds_one_endpoint_only() {
    let rl = IntroRateLimiter::with_caps(1, 100, 3_600_000, 300_000);
    assert!(rl.admit_connection([1; 32], 0).is_ok());
    assert_eq!(rl.admit_connection([1; 32], 1), Err("per-connection cap"), "same endpoint shed at cap");
    assert!(rl.admit_connection([2; 32], 2).is_ok(), "a different endpoint still admits");
}

#[test]
fn limiter_dedupes_same_pair_within_ttl_per_role() {
    let rl = IntroRateLimiter::with_caps(100, 100, 3_600_000, 300_000);
    let v = OwnerAddr([1; 16]);
    let s = OwnerAddr([2; 16]);
    assert!(rl.admit_voucher(v, s, 0).is_ok());
    assert_eq!(rl.admit_voucher(v, s, 1), Err("duplicate"), "same (voucher,subject) within ttl is deduped");
    assert!(rl.admit_voucher(v, OwnerAddr([9; 16]), 2).is_ok(), "different subject is not a duplicate");
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(limiter_)'`
Expected: FAIL — `no function with_caps` / `no method admit_connection`.

- [ ] **Step 3: Implement the container**

Add the new const near the others:
```rust
/// Pre-auth per-connection-endpoint cap over the same 1h window. Generous vs the
/// per-owner 20 because one iroh endpoint may legitimately host/relay for several
/// owners; a genuine single-endpoint flood is still shed.
pub const INTRO_PER_CONNECTION_MAX: usize = 40;
```

Replace `IntroRateLimiter` + `IntroRateLimiterInner` + `impl` (struct `:594`, methods through `:745`) with:

```rust
/// ZEB-694: two-tier introduction rate limiter.
/// - Tier 1 (`admit_connection`): pre-auth flood shield keyed on the connecting
///   iroh endpoint's authenticated `remote_id()` — un-spoofable, runs before any
///   signature verification.
/// - Tier 2 (`admit_requester` / `admit_voucher`): post-auth per-owner quotas +
///   dedupe, keyed on the AUTHENTICATED owner, in DISJOINT per-role namespaces so
///   requester traffic and voucher traffic never share a budget.
pub struct IntroRateLimiter {
    inner: Mutex<Inner>,
}

struct Inner {
    conn: KeyedSlidingWindow<[u8; 32]>,
    req_window: KeyedSlidingWindow<OwnerAddr>,
    req_dedupe: KeyedDedupe<(OwnerAddr, OwnerAddr)>,
    vouch_window: KeyedSlidingWindow<OwnerAddr>,
    vouch_dedupe: KeyedDedupe<(OwnerAddr, OwnerAddr)>,
}

impl IntroRateLimiter {
    pub fn new() -> Self {
        Self::with_caps(
            INTRO_PER_CONNECTION_MAX,
            INTRO_PER_VOUCHER_MAX,
            INTRO_PER_VOUCHER_WINDOW_MS,
            INTRO_DEDUPE_TTL_MS,
        )
    }

    /// Test/tuning constructor — deterministic tiny caps in unit tests.
    pub fn with_caps(conn_max: usize, per_owner_max: usize, window_ms: u64, dedupe_ttl_ms: u64) -> Self {
        Self {
            inner: Mutex::new(Inner {
                conn: KeyedSlidingWindow::new(conn_max, window_ms),
                req_window: KeyedSlidingWindow::new(per_owner_max, window_ms),
                req_dedupe: KeyedDedupe::new(dedupe_ttl_ms),
                vouch_window: KeyedSlidingWindow::new(per_owner_max, window_ms),
                vouch_dedupe: KeyedDedupe::new(dedupe_ttl_ms),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Tier 1 — pre-auth. Key = the connecting endpoint's authenticated `remote_id`.
    pub fn admit_connection(&self, remote_id: [u8; 32], now_ms: u64) -> Result<(), &'static str> {
        if self.lock().conn.admit(remote_id, now_ms) {
            Ok(())
        } else {
            Err("per-connection cap")
        }
    }

    /// Tier 2 — post-auth requester quota. Key = the AUTHENTICATED requester.
    pub fn admit_requester(&self, requester: OwnerAddr, target: OwnerAddr, now_ms: u64) -> Result<(), &'static str> {
        let mut inner = self.lock();
        if inner.req_dedupe.is_duplicate((requester, target), now_ms) {
            return Err("duplicate");
        }
        if !inner.req_window.admit(requester, now_ms) {
            return Err("per-requester cap");
        }
        inner.req_dedupe.record((requester, target), now_ms);
        Ok(())
    }

    /// Tier 2 — post-auth voucher quota. Key = the VERIFIED voucher.
    pub fn admit_voucher(&self, voucher: OwnerAddr, subject: OwnerAddr, now_ms: u64) -> Result<(), &'static str> {
        let mut inner = self.lock();
        if inner.vouch_dedupe.is_duplicate((voucher, subject), now_ms) {
            return Err("duplicate");
        }
        if !inner.vouch_window.admit(voucher, now_ms) {
            return Err("per-voucher cap");
        }
        inner.vouch_dedupe.record((voucher, subject), now_ms);
        Ok(())
    }

    /// Test helper: (voucher-role dedupe entries, voucher-role window keys). The
    /// migrated Task-13 flood tests exercise `admit_voucher`, so they assert on the
    /// voucher role's maps. Same-module access to the primitives' private fields.
    #[cfg(test)]
    pub(crate) fn tracked_len(&self) -> (usize, usize) {
        let inner = self.lock();
        (inner.vouch_dedupe.last_seen.len(), inner.vouch_window.windows.len())
    }
}

impl Default for IntroRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}
```
(If the existing `tracked_len` is not `#[cfg(test)]`, match its existing visibility/gating; only its BODY changes to read the voucher-role maps.)

Notes: the borrow-split in `admit_requester`/`admit_voucher` (calling `is_duplicate` then `admit` then `record` on distinct fields of `*inner`) compiles because they touch different fields; if the borrow checker objects, split into `let inner = &mut *self.lock();` and access `inner.req_dedupe` / `inner.req_window` as separate field borrows. `OwnerAddr` and `[u8; 32]` are `Copy + Eq + Hash` (existing derives), satisfying the primitive bound.

- [ ] **Step 4: Migrate the five existing Task-13 tests to `admit_voucher`**

In `friend_intro.rs` (`:1284-1418`), the five `intro_rate_limiter_*` tests call the removed `admit`. They are voucher-shaped (key + subject), so migrate each call `rl.admit(k, s, t)` → `rl.admit_voucher(k, s, t)`. Semantics and error strings are identical (`"duplicate"`, `"per-voucher cap"`), so the assertions stand unchanged. The two flood tests (`intro_rate_limiter_bounds_memory_under_rotating_flood`, `intro_rate_limiter_eviction_preserves_legit_sequence`) keep using `tracked_len()` — now reporting the voucher-role maps, which is exactly what `admit_voucher` populates. Do NOT weaken these tests; they are the 8192-cap regression at the limiter level.

- [ ] **Step 5: Rewire both acceptor arms (`iroh_pex_acceptor.rs`)**

Broker/requester arm — replace the pre-auth block at `:544-557` (`self.intro_rate_limiter.admit(ir.from_addr, ir.target, wall_now_ms())`) with one `now` + the connection shield:
```rust
    let now = wall_now_ms();
    // ZEB-694 Tier 1 (pre-auth flood shield): key on the connecting endpoint's
    // authenticated iroh id — un-spoofable, before any verification.
    if let Err(reason) = self.intro_rate_limiter.admit_connection(*conn.remote_id().as_bytes(), now) {
        tracing::warn!(reason, "introduction shed by connection shield");
        return self.write_ack(&mut send).await;
    }
```
Then IMMEDIATELY AFTER `authenticate_introduce_request(&ir, self.self_owner, now_secs)` succeeds (`:578`), add the post-auth requester quota:
```rust
    // ZEB-694 Tier 2 (post-auth): `ir.from_addr` is now authenticated.
    if let Err(reason) = self.intro_rate_limiter.admit_requester(ir.from_addr, ir.target, now) {
        tracing::warn!(reason, key = %hex::encode(ir.from_addr.0), "introduction shed by requester quota");
        return self.write_ack(&mut send).await;
    }
```

Voucher/target arm — replace the pre-auth block at `:669-683` (`self.intro_rate_limiter.admit(intro.voucher, intro.subject, wall_now_ms())`) with:
```rust
    let now = wall_now_ms();
    // ZEB-694 Tier 1: the connecting endpoint here is the DELIVERER (F dialing X).
    if let Err(reason) = self.intro_rate_limiter.admit_connection(*conn.remote_id().as_bytes(), now) {
        tracing::warn!(reason, "introduction shed by connection shield");
        return self.write_ack(&mut send).await;
    }
```
Then IMMEDIATELY AFTER `verify_introduction(...)` succeeds (`:711`), add:
```rust
    // ZEB-694 Tier 2 (post-auth): `intro.voucher` is now verified.
    if let Err(reason) = self.intro_rate_limiter.admit_voucher(intro.voucher, intro.subject, now) {
        tracing::warn!(reason, key = %hex::encode(intro.voucher.0), "introduction shed by voucher quota");
        return self.write_ack(&mut send).await;
    }
```
Match the exact local `send` variable the surrounding `write_ack(&mut send)` calls use; reuse the existing `now_secs` for the auth call. Confirm the post-auth insert lands AFTER the verify/auth returns Ok and BEFORE the reachability/policy/dial work, so a shed still does no dial.

- [ ] **Step 6: Run limiter unit tests + e2e regression + lib clippy (FOREGROUND)**

```bash
cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(limiter_) | test(keyed_) | test(intro_rate_limiter)'
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(introduction_broker_roundtrip)' --test-threads 1
cd src-tauri && cargo fmt --all && cargo clippy --locked --lib --features test-fixtures --no-deps -- -D warnings
```
Expected: all limiter tests PASS (7 new/migrated + 5 Task-13); the 3-node e2e PASS (production caps 40/conn, 20/role are far above the handful of frames it sends); clippy clean. (`--lib` clippy avoids relinking all integration binaries; the final sweep runs `--all-targets`.)

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/friend_intro.rs src-tauri/src/iroh_pex_acceptor.rs
git commit -m "feat(zeb-694): two-tier role-separated IntroRateLimiter + acceptor rewire (conn shield pre-auth, owner quotas post-auth)"
```

---

## Group B — Accept path

### Task B1: `peek_offer` — non-consuming offer read

**Files:**
- Modify: `src-tauri/src/friend_requests.rs` (near `take_offer` `:143` / `has_offer` `:163`)
- Test: inline `#[cfg(test)]` in `src-tauri/src/friend_requests.rs`

**Interfaces:**
- Produces: `pub fn peek_offer(&self, subject: &OwnerAddr) -> Option<(StoredIntroductionOffer, u64)>` — a clone of the staged offer plus its `received_at_ms`; `None` if absent or a plain `LinkRequest`. (`StoredIntroductionOffer` and `ReachabilityAnnouncePayload` both derive `Clone`.)

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn peek_offer_clones_without_consuming() {
    let store = PendingFriendRequests::default();
    let subj = OwnerAddr([1; 16]);
    let offer = StoredIntroductionOffer {
        voucher: OwnerAddr([2; 16]),
        subject: subj,
        reachability: fixture_reach(), // existing helper in this test module (:308)
    };
    store.record_introduction_offer(subj, Some("x".into()), 4242, offer.clone());
    let (peeked, received_at) = store.peek_offer(&subj).expect("offer present");
    assert_eq!(peeked, offer);
    assert_eq!(received_at, 4242);
    assert!(store.has_offer(&subj), "peek did NOT consume the offer");
    // a plain LinkRequest yields None
    let other = OwnerAddr([9; 16]);
    store.record_inbound(other, None, 1);
    assert!(store.peek_offer(&other).is_none(), "a LinkRequest is not an offer");
}
```

Note: `ReachabilityAnnouncePayload` has NO `Default`. Use the existing `fixture_reach()` helper already in this test module (`friend_requests.rs:308` — builds the 7-field zeroed payload) and the existing `addr(n)` helper / `OwnerAddr([n; 16])` literals; do not invent fields or add a `Default`.

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(peek_offer)'`
Expected: FAIL — `no method peek_offer`.

- [ ] **Step 3: Implement**

```rust
/// Non-consuming clone of a staged `IntroductionOffer` plus its `received_at_ms`.
/// Returns `None` if the entry is absent or a plain `LinkRequest`. The accept
/// path uses this (instead of `take_offer`) so a failed dial leaves the offer
/// staged for retry (ZEB-694).
pub fn peek_offer(&self, subject: &OwnerAddr) -> Option<(StoredIntroductionOffer, u64)> {
    let inner = self.inner.lock().expect("pending inner mutex poisoned");
    match inner.inbound.get(subject) {
        Some(p) => match &p.kind {
            PendingKind::IntroductionOffer(o) => Some(((**o).clone(), p.received_at_ms)),
            PendingKind::LinkRequest => None,
        },
        None => None,
    }
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(peek_offer)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/friend_requests.rs
git commit -m "feat(zeb-694): non-consuming peek_offer on PendingFriendRequests"
```

---

### Task B2: In-flight accept guard

**Files:**
- Modify: `src-tauri/src/friend_requests.rs` — add `accepting: HashSet<OwnerAddr>` to `Inner` (`:65-71`); add methods + `AcceptInFlightGuard`.
- Test: inline `#[cfg(test)]`

**Interfaces:**
- Produces:
  - `pub fn try_begin_accept(&self, subject: OwnerAddr) -> bool` — `false` if an accept for `subject` is already in flight; else marks in-flight and returns `true`.
  - `pub fn end_accept(&self, subject: &OwnerAddr)` — clears the marker.
  - `pub struct AcceptInFlightGuard` with `pub fn new(store: Arc<PendingFriendRequests>, subject: OwnerAddr) -> Self`; its `Drop` calls `end_accept`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn in_flight_guard_blocks_concurrent_accept() {
    use std::sync::Arc;
    let store = Arc::new(PendingFriendRequests::default());
    let subj = OwnerAddr([1; 16]);
    assert!(store.try_begin_accept(subj), "first accept begins");
    assert!(!store.try_begin_accept(subj), "second concurrent accept is blocked");
    {
        // RAII guard clears the marker on drop.
        let _g = AcceptInFlightGuard::new(Arc::clone(&store), subj);
        // still in flight while the guard lives
        assert!(!store.try_begin_accept(subj));
    } // _g drops here → end_accept
    // NOTE: try_begin_accept above set the flag; the guard's drop cleared it.
    assert!(store.try_begin_accept(subj), "after the guard drops, a new accept can begin");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(in_flight_guard)'`
Expected: FAIL — `no method try_begin_accept` / `cannot find AcceptInFlightGuard`.

- [ ] **Step 3: Implement**

Add the field to `Inner`:
```rust
#[derive(Default)]
struct Inner {
    inbound: HashMap<OwnerAddr, PendingInbound>,
    approved: HashSet<OwnerAddr>,
    /// ZEB-694: subjects with an introduction accept currently dialing — blocks a
    /// concurrent second accept from double-dialing.
    accepting: HashSet<OwnerAddr>,
}
```

Add methods + guard (ensure `use std::sync::Arc;` and `use std::collections::HashSet;` are present):
```rust
/// Test-and-set: `true` and marks in-flight if no accept for `subject` is
/// already dialing; `false` if one is. Pair with `end_accept` (or the RAII
/// `AcceptInFlightGuard`).
pub fn try_begin_accept(&self, subject: OwnerAddr) -> bool {
    let mut inner = self.inner.lock().expect("pending inner mutex poisoned");
    inner.accepting.insert(subject) // HashSet::insert returns false if already present
}

/// Clear the in-flight marker for `subject`.
pub fn end_accept(&self, subject: &OwnerAddr) {
    let mut inner = self.inner.lock().expect("pending inner mutex poisoned");
    inner.accepting.remove(subject);
}

/// RAII: clears the in-flight accept marker on drop, so every accept exit path
/// (early return, dial error, panic) releases it.
pub struct AcceptInFlightGuard {
    store: Arc<PendingFriendRequests>,
    subject: OwnerAddr,
}

impl AcceptInFlightGuard {
    pub fn new(store: Arc<PendingFriendRequests>, subject: OwnerAddr) -> Self {
        Self { store, subject }
    }
}

impl Drop for AcceptInFlightGuard {
    fn drop(&mut self) {
        self.store.end_accept(&self.subject);
    }
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(in_flight_guard)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/friend_requests.rs
git commit -m "feat(zeb-694): in-flight accept guard (try_begin_accept + AcceptInFlightGuard RAII)"
```

---

### Task B3: Offer TTL — const, expiry check, sweep, list wiring

**Files:**
- Modify: `src-tauri/src/friend_requests.rs` — add `INTRODUCTION_OFFER_TTL_MS`, `is_offer_expired`, `sweep_expired_offers`.
- Modify: `src-tauri/src/lib.rs` — thread `now_ms` through `list_pending_friend_requests_inner` (`:54531`) and sweep before projecting; its caller `list_pending_friend_requests_impl` (`:54589`) passes `wall_now_ms()`.
- Test: inline `#[cfg(test)]` in `friend_requests.rs`.

**Interfaces:**
- Produces:
  - `pub const INTRODUCTION_OFFER_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;`
  - `pub fn is_offer_expired(received_at_ms: u64, now_ms: u64) -> bool`
  - `pub fn sweep_expired_offers(&self, now_ms: u64) -> usize` (returns count swept)
- Changes: `list_pending_friend_requests_inner(store: &PendingFriendRequests, now_ms: u64) -> Vec<PendingFriendRequestDto>` (added `now_ms`).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn sweep_removes_only_expired_offers() {
    let store = PendingFriendRequests::default();
    let fresh = OwnerAddr([1; 16]);
    let stale = OwnerAddr([2; 16]);
    let link = OwnerAddr([3; 16]);
    let mk = |s: OwnerAddr| StoredIntroductionOffer {
        voucher: OwnerAddr([9; 16]),
        subject: s,
        reachability: fixture_reach(), // existing helper in this test module (friend_requests.rs:308)
    };
    let now = 10 * INTRODUCTION_OFFER_TTL_MS;
    store.record_introduction_offer(fresh, None, now, mk(fresh)); // received now → fresh
    store.record_introduction_offer(stale, None, now - INTRODUCTION_OFFER_TTL_MS, mk(stale)); // exactly TTL old → expired
    store.record_inbound(link, None, 0); // a LinkRequest, never swept

    assert!(is_offer_expired(now - INTRODUCTION_OFFER_TTL_MS, now));
    assert!(!is_offer_expired(now, now));

    let swept = store.sweep_expired_offers(now);
    assert_eq!(swept, 1, "only the stale offer is swept");
    assert!(store.has_offer(&fresh), "fresh offer retained");
    assert!(!store.has_offer(&stale), "stale offer removed");
    // LinkRequest untouched (peek_offer is None for it, but the inbound entry remains)
    assert!(store.peek_offer(&link).is_none());
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(sweep_removes_only_expired)'`
Expected: FAIL — `cannot find INTRODUCTION_OFFER_TTL_MS` / `no method sweep_expired_offers`.

- [ ] **Step 3: Implement in `friend_requests.rs`**

```rust
/// ZEB-694: a staged AskMe offer older than this is treated as dead (its relayed
/// reachability is past the intro/reachability freshness bound anyway) — swept
/// from the inbox and rejected at accept time with an "expired" message.
pub const INTRODUCTION_OFFER_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000; // 7d

pub fn is_offer_expired(received_at_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(received_at_ms) >= INTRODUCTION_OFFER_TTL_MS
}

impl PendingFriendRequests {
    /// Remove every staged `IntroductionOffer` older than the TTL. Plain
    /// `LinkRequest` entries have their own lifecycle and are left untouched.
    /// Returns the number of offers swept.
    pub fn sweep_expired_offers(&self, now_ms: u64) -> usize {
        let mut inner = self.inner.lock().expect("pending inner mutex poisoned");
        let before = inner.inbound.len();
        inner.inbound.retain(|_, p| match p.kind {
            PendingKind::IntroductionOffer(_) => !is_offer_expired(p.received_at_ms, now_ms),
            PendingKind::LinkRequest => true,
        });
        before - inner.inbound.len()
    }
}
```

(Place `sweep_expired_offers` inside an existing `impl PendingFriendRequests` block rather than a fresh one if the file uses a single block.)

- [ ] **Step 4: Wire the sweep into the list path (`lib.rs`)**

Change `list_pending_friend_requests_inner` (`:54531`) to accept `now_ms` and sweep first:
```rust
pub fn list_pending_friend_requests_inner(
    store: &crate::friend_requests::PendingFriendRequests,
    now_ms: u64,
) -> Vec<PendingFriendRequestDto> {
    // ZEB-694: drop offers past the TTL so the UI stops showing dead introductions.
    store.sweep_expired_offers(now_ms);
    store
        .list()
        .into_iter()
        // ... unchanged map/collect ...
}
```
Update the sole caller `list_pending_friend_requests_impl` (`:54589`):
```rust
    Ok(list_pending_friend_requests_inner(&store, wall_now_ms()))
```
If any test calls `list_pending_friend_requests_inner` with one arg, update it to pass an explicit `now_ms`.

- [ ] **Step 5: Run tests + lib clippy**

Run: `cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(sweep_removes_only_expired) | test(peek_offer) | test(in_flight_guard)' && cargo clippy --locked --lib --features test-fixtures --no-deps -- -D warnings`
Expected: PASS + clean.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/friend_requests.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-694): 7d TTL on staged offers + lazy-sweep in list path"
```

---

### Task B4: Rework the accept branch + accept-branch coverage (ZEB-693 Gap 2)

**Files:**
- Modify: `src-tauri/src/lib.rs` — add `IntroAcceptGate`, `begin_introduction_accept`, `finalize_introduction_accept`; rewrite the `IntroductionOffer` branch of `accept_friend_request_impl` (`:54669-54749`).
- Test: inline `#[cfg(test)]` in `lib.rs` (new module `zeb694_accept_tests`).

**Interfaces:**
- Consumes: `peek_offer` (B1), `try_begin_accept`/`AcceptInFlightGuard` (B2), `is_offer_expired` (B3), existing `complete_introduction` + `AddFriendOutcome` (`lib.rs:55344`), existing `wall_now_ms()`.
- Produces (both `pub(crate)` so the in-crate test module reaches them; both `NodeState`-free):
  - `enum IntroAcceptGate { Proceed { offer: StoredIntroductionOffer, guard: AcceptInFlightGuard }, AlreadyInFlight, Expired, Vanished }`
  - `fn begin_introduction_accept(store: &Arc<PendingFriendRequests>, addr: OwnerAddr, now_ms: u64) -> IntroAcceptGate`
  - `fn finalize_introduction_accept(store: &PendingFriendRequests, addr: OwnerAddr, result: Result<AddFriendOutcome, String>) -> Result<(), String>`

- [ ] **Step 1: Write the failing tests (ZEB-693 Gap 2 coverage)**

Add a module to `lib.rs`:
```rust
#[cfg(test)]
mod zeb694_accept_tests {
    use super::*;
    use crate::friend_requests::{PendingFriendRequests, StoredIntroductionOffer, INTRODUCTION_OFFER_TTL_MS};
    use crate::owner_state_types::OwnerAddr;
    use std::sync::Arc;

    // ReachabilityAnnouncePayload has no Default; build the 7-field zeroed payload
    // (mirrors friend_requests.rs:308 fixture_reach — the store never inspects it).
    fn fixture_reach() -> crate::reachability_record::ReachabilityAnnouncePayload {
        crate::reachability_record::ReachabilityAnnouncePayload {
            iroh_node_id: [7u8; 32],
            home_relay_url: String::new(),
            direct_addresses: Vec::new(),
            announced_at_ms: 0,
            identity_signature: [0u8; 64],
            butler_set: Vec::new(),
            bs_at: 0,
        }
    }

    fn stage(store: &PendingFriendRequests, subj: OwnerAddr, received_at: u64) {
        store.record_introduction_offer(
            subj,
            None,
            received_at,
            StoredIntroductionOffer {
                voucher: OwnerAddr([9; 16]),
                subject: subj,
                reachability: fixture_reach(),
            },
        );
    }

    #[test]
    fn begin_gate_proceeds_then_blocks_concurrent() {
        let store = Arc::new(PendingFriendRequests::default());
        let subj = OwnerAddr([1; 16]);
        stage(&store, subj, 1000);
        let gate = begin_introduction_accept(&store, subj, 2000);
        assert!(matches!(gate, IntroAcceptGate::Proceed { .. }));
        // guard held by `gate` → a second begin is blocked
        assert!(matches!(begin_introduction_accept(&store, subj, 2000), IntroAcceptGate::AlreadyInFlight));
        drop(gate); // releases the guard
        assert!(matches!(begin_introduction_accept(&store, subj, 2000), IntroAcceptGate::Proceed { .. }));
    }

    #[test]
    fn begin_gate_expires_stale_offer() {
        let store = Arc::new(PendingFriendRequests::default());
        let subj = OwnerAddr([1; 16]);
        stage(&store, subj, 0);
        let gate = begin_introduction_accept(&store, subj, INTRODUCTION_OFFER_TTL_MS + 1);
        assert!(matches!(gate, IntroAcceptGate::Expired));
        assert!(!store.has_offer(&subj), "expired offer dropped");
    }

    #[test]
    fn begin_gate_vanished_when_no_offer() {
        let store = Arc::new(PendingFriendRequests::default());
        let subj = OwnerAddr([1; 16]);
        assert!(matches!(begin_introduction_accept(&store, subj, 1), IntroAcceptGate::Vanished));
    }

    #[test]
    fn finalize_consumes_only_on_linked() {
        let store = PendingFriendRequests::default();
        let subj = OwnerAddr([1; 16]);
        // Linked → consumed + Ok
        stage(&store, subj, 0);
        let r = finalize_introduction_accept(&store, subj,
            Ok(AddFriendOutcome::Linked { owner_id_hex: "aa".into(), display: None }));
        assert!(r.is_ok());
        assert!(!store.has_offer(&subj), "Linked consumes the offer");
        // Unreachable → retained + Err
        stage(&store, subj, 0);
        let r = finalize_introduction_accept(&store, subj, Ok(AddFriendOutcome::Unreachable));
        assert!(r.is_err());
        assert!(store.has_offer(&subj), "Unreachable retains the offer for retry");
        // Pending → retained + Err
        let r = finalize_introduction_accept(&store, subj, Ok(AddFriendOutcome::Pending));
        assert!(r.is_err());
        assert!(store.has_offer(&subj));
        // dial Err → retained + propagates message
        let r = finalize_introduction_accept(&store, subj, Err("boom".into()));
        assert_eq!(r, Err("boom".into()));
        assert!(store.has_offer(&subj));
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(zeb694_accept)'`
Expected: FAIL — the three fns don't exist.

- [ ] **Step 3: Implement the seams**

Add near `accept_friend_request_impl` in `lib.rs`:
```rust
use crate::friend_requests::{AcceptInFlightGuard, PendingFriendRequests, StoredIntroductionOffer};

/// ZEB-694: outcome of the accept-branch entry gate (guard + peek + TTL), decided
/// with only the store + clock (no NodeState).
pub(crate) enum IntroAcceptGate {
    Proceed { offer: StoredIntroductionOffer, guard: AcceptInFlightGuard },
    AlreadyInFlight,
    Expired,
    Vanished,
}

/// Acquire the in-flight guard, peek the staged offer, and apply the TTL — WITHOUT
/// consuming the offer. On `Proceed` the returned `guard` must be held across the
/// dial so a concurrent accept can't double-dial; it clears the marker on drop.
pub(crate) fn begin_introduction_accept(
    store: &std::sync::Arc<PendingFriendRequests>,
    addr: crate::owner_state_types::OwnerAddr,
    now_ms: u64,
) -> IntroAcceptGate {
    if !store.try_begin_accept(addr) {
        return IntroAcceptGate::AlreadyInFlight;
    }
    let guard = AcceptInFlightGuard::new(std::sync::Arc::clone(store), addr);
    let Some((offer, received_at)) = store.peek_offer(&addr) else {
        return IntroAcceptGate::Vanished; // guard drops → clears marker
    };
    if crate::friend_requests::is_offer_expired(received_at, now_ms) {
        store.take_offer(&addr); // drop the dead entry
        return IntroAcceptGate::Expired; // guard drops → clears marker
    }
    IntroAcceptGate::Proceed { offer, guard }
}

/// Consume the staged offer IFF the dial reached `Linked`; otherwise leave it
/// staged for retry and surface a distinguishable message.
pub(crate) fn finalize_introduction_accept(
    store: &PendingFriendRequests,
    addr: crate::owner_state_types::OwnerAddr,
    result: Result<AddFriendOutcome, String>,
) -> Result<(), String> {
    match result {
        Ok(AddFriendOutcome::Linked { .. }) => {
            store.take_offer(&addr);
            Ok(())
        }
        Ok(AddFriendOutcome::Pending) | Ok(AddFriendOutcome::Unreachable) => {
            Err("Couldn't reach them right now — the introduction is saved, try Accept again later.".into())
        }
        Err(e) => Err(e),
    }
}
```

- [ ] **Step 4: Rewrite the `IntroductionOffer` branch of `accept_friend_request_impl`**

Replace the branch body (`:54669-54749`, from `if store.has_offer(&addr) {` through the `return result.map(|_outcome| ());`) with:
```rust
    if store.has_offer(&addr) {
        let (offer, _accept_guard) = match begin_introduction_accept(&store, addr, wall_now_ms()) {
            IntroAcceptGate::Proceed { offer, guard } => (offer, guard),
            IntroAcceptGate::AlreadyInFlight => {
                crate::node_event_sink::emit_ser(sink.as_ref(), "friend-list-changed", &());
                return Err("This introduction is already being accepted.".into());
            }
            IntroAcceptGate::Expired => {
                crate::node_event_sink::emit_ser(sink.as_ref(), "friend-list-changed", &());
                return Err("This introduction has expired — ask them for a fresh one.".into());
            }
            IntroAcceptGate::Vanished => {
                crate::node_event_sink::emit_ser(sink.as_ref(), "friend-list-changed", &());
                return Err(OWNER_NOT_LOADED_MSG.into());
            }
        };

        // Validate the self-dial/durability handles (unchanged) — a missing handle
        // returns without consuming (the guard clears the in-flight marker on drop,
        // the offer stays staged).
        let (
            Some(iroh_endpoint),
            Some(crdt_state),
            Some(hlc_tracker),
            Some(device_id),
            Some(self_owner),
            Some(dm_outbox),
            Some(owner_keytree),
        ) = (
            iroh_endpoint,
            crdt_state,
            hlc_tracker,
            device_id,
            self_owner,
            dm_outbox,
            owner_keytree,
        )
        else {
            crate::node_event_sink::emit_ser(sink.as_ref(), "friend-list-changed", &());
            return Err(OWNER_NOT_LOADED_MSG.into());
        };

        let (device2_key, enrollment_cert) = {
            let o = dm_outbox.lock().await;
            (
                std::sync::Arc::clone(&o.community_signing_key),
                o.enrollment_cert.clone(),
            )
        };
        let self_reachability = build_self_handshake_reachability(
            self_identity_pub_64,
            self_dsa_pubkey,
            self_kem_pubkey,
            Some(&iroh_endpoint),
        );

        let result = complete_introduction(
            offer.subject,
            offer.reachability,
            iroh_endpoint,
            HandshakeDialConfig::from_env(),
            self_owner,
            None,
            enrollment_cert,
            device2_key,
            self_reachability,
            owner_keytree,
            crdt_state,
            hlc_tracker,
            device_id,
            sync_engine,
            friend_publisher,
            Some(std::sync::Arc::clone(&sink)),
        )
        .await;

        // ZEB-694: consume the offer ONLY on `Linked`; otherwise it stays staged
        // for retry. `_accept_guard` drops at function exit, clearing the marker.
        let outcome = finalize_introduction_accept(&store, addr, result);
        crate::node_event_sink::emit_ser(sink.as_ref(), "friend-list-changed", &());
        return outcome;
    }
```

Note: `_accept_guard` is `_`-prefixed to suppress the unused-binding lint while still running `Drop` at scope end. `store` is `Arc<PendingFriendRequests>` here (unwrapped at `:54656`), satisfying `begin_introduction_accept`'s `&Arc<...>` parameter.

- [ ] **Step 5: Run tests + lib clippy**

Run: `cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(zeb694_accept)' && cargo clippy --locked --lib --features test-fixtures --no-deps -- -D warnings`
Expected: PASS (4 tests) + clean.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-694): consume-offer-on-Linked accept path + in-crate branch coverage (closes ZEB-693 Gap 2)"
```

---

### Task B5: Frontend — keep the request row on a non-linked accept

**Files:**
- Modify: `src/lib/friend-service.ts` (the `acceptFriendRequest` wrapper), `src/lib/components/FriendsPanel.svelte` (the accept-button handler)
- Test: `src/lib/friend-service.test.ts` (or the existing FriendsPanel/friend-service vitest spec — reuse the file that already tests accept)

**Interfaces:**
- Consumes: the `accept_friend_request` IPC now returns `Ok(())` on `Linked` and `Err(message)` on not-linked/expired/already-in-flight; on `Err` the offer remains staged so a fresh `list_pending_friend_requests` still includes the row.

- [ ] **Step 1: Write the failing test**

In the friend-service spec (mirror the existing accept test's mock setup):
```typescript
it('keeps the pending request when accept is rejected (not linked)', async () => {
  // invoke('accept_friend_request', …) rejects with a backend message
  mockInvoke.mockRejectedValueOnce('Couldn\'t reach them right now — the introduction is saved, try Accept again later.');
  await expect(acceptFriendRequest(OWNER_HEX)).rejects.toThrow(/introduction is saved/);
  // the service must NOT have optimistically removed the row from its store
  expect(getPendingRequests()).toContainEqual(expect.objectContaining({ ownerIdHex: OWNER_HEX }));
});
```
(Adapt names to the actual `friend-service.ts` exports — reuse the accessor/store the existing accept test uses. If the service has no local pending store and the panel drives removal purely from `friend-list-changed` refetch, assert instead that the handler surfaces the error and does not call a local remove — see Step 3.)

- [ ] **Step 2: Run to verify it fails**

Run: `npx vitest run src/lib/friend-service.test.ts`
Expected: FAIL (the current handler removes the row optimistically, or swallows the error).

- [ ] **Step 3: Implement**

In `friend-service.ts` / `FriendsPanel.svelte`, ensure the accept handler:
- awaits `invoke('accept_friend_request', { ownerIdHex })`;
- on rejection, extracts the message (`const msg = e instanceof Error ? e.message : String(e);`) and surfaces it (toast/inline error) — does NOT remove the request row locally;
- drives row removal ONLY from the `friend-list-changed` event → refetch `list_pending_friend_requests` (which, post-B4, still contains the row on a non-linked accept, and drops it once linked or expired).

If the handler currently does an optimistic local removal before/independent of the refetch, delete that optimistic removal.

- [ ] **Step 4: Run to verify it passes + typecheck**

Run: `npx vitest run src/lib/friend-service.test.ts && npx tsc --noEmit`
Expected: PASS + clean.

- [ ] **Step 5: Commit**

```bash
git add src/lib/friend-service.ts src/lib/components/FriendsPanel.svelte src/lib/friend-service.test.ts
git commit -m "feat(zeb-694): keep pending row on a non-linked introduction accept + surface message"
```

---

## Final whole-branch gate (controller, before PR)

After all tasks + the whole-branch review, run the CI-parity full sweep FOREGROUND (single blocking call each):

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
# repo root:
npx tsc --noEmit && npx vitest run
```
Plus confirm the wire fixtures are byte-identical: `git diff --stat -- src-tauri/tests/wire_format/zeb375_pex_fixtures.rs src-tauri/tests/wire_format/zeb376_intro_fixtures.rs` must be EMPTY.

---

## Self-Review

**Spec coverage:** Part 1 (limiter) → A1 (window primitive), A2 (dedupe primitive), A3 (ATOMIC: two-tier container + role-independence + conn-shield tests + five migrated Task-13 tests + both acceptor arms rewired at the auth boundary + e2e regression). Part 2 (accept path) → B1 (peek), B2 (guard), B3 (TTL + sweep + list wiring), B4 (consume-on-`Linked` seams + branch rewrite + Gap-2 coverage), B5 (frontend keep-row). Global constraints: no-wire-change asserted in the final gate; benign-ack preserved in A3's acceptor rewire; 8192-cap eviction preserved in A1/A2 (primitive-level) + the migrated flood tests (limiter-level); `Linked`-gated durability untouched (B4 leaves `complete_introduction` and its call args verbatim). ZEB-693 Gap 2 closed by B4's `begin_/finalize_introduction_accept` unit coverage. **Note:** A3 is compilation-atomic — removing `admit` breaks the acceptor + 5 tests in one compile unit, so restructure + migrate + rewire land together (8 tasks total: A1–A3, B1–B5).

**Placeholder scan:** No TBD/TODO. The two "reuse the existing test helper" notes (ReachabilityAnnouncePayload construction in B1/B4, the frontend accept test shape in B5) point at concrete existing code the implementer reads, not invented content — flagged because the exact helper name lives in files outside this plan's excerpts.

**Type consistency:** `admit_connection([u8;32])`, `admit_requester`/`admit_voucher(OwnerAddr, OwnerAddr)` consistent A3↔A4. `peek_offer -> Option<(StoredIntroductionOffer, u64)>` consistent B1↔B4. `IntroAcceptGate`/`begin_introduction_accept`/`finalize_introduction_accept` signatures consistent B4 body↔tests. `list_pending_friend_requests_inner(store, now_ms)` consistent B3 impl↔caller. `AddFriendOutcome::{Linked,Pending,Unreachable}` matches the definition at `lib.rs:55344`.
