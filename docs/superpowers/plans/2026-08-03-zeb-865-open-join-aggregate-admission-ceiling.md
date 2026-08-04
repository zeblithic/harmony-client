# ZEB-865 Node-wide Aggregate Open-Join Admission Ceiling — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a node-wide aggregate admission ceiling (1024/60 s) to the open-join path, in addition to the existing per-source budget, bounding a Sybil fan-out's expensive materialization work without reintroducing the pre-B7 global-lockout DoS.

**Architecture:** A single unit-key sliding window (`global: KeyedSlidingWindow<()>`) inside `OpenJoinRateLimiter`, checked via a new non-mutating `would_admit` peek before the per-source `allow` records and committed only after `allow` admits. The whole step-7 decision is encapsulated in one method `admit_source`, which `verify_and_admit_open_join` calls. The acceptor is untouched.

**Tech Stack:** Rust, `cargo nextest`, the audited `friend_intro::KeyedSlidingWindow` primitive.

## Global Constraints

- Ceiling value: `OPEN_JOIN_GLOBAL_ADMIT_MAX = 1024` admissions per `OPEN_JOIN_RATE_LIMIT_WINDOW_MS` (60 s). Verbatim.
- The aggregate ceiling sheds **only fully-verified requests** (checked at step 7, after the capability MAC + device-hash + enrollment + ed25519 `verify_strict`) — never move it before signature verification.
- Existing per-source behavior must stay byte-identical: `allow`, `is_replay`, `record_nonce` are unchanged; the global cap (1024) is far above the per-source cap (20).
- Reject variant `NodeCapacity` flows through the existing `write_open_join_rejection` typed-rejection path — no acceptor change.
- Cargo commands run from `src-tauri/`. Always `--locked --features test-fixtures`. Clippy uses `--all-targets`.
- cwd drifts between shell calls — `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri` inside each command.

---

### Task 1: `KeyedSlidingWindow::would_admit` — non-mutating capacity peek

**Files:**
- Modify: `src-tauri/src/friend_intro.rs` (after `KeyedSlidingWindow::admit`, ~line 645)
- Test: `src-tauri/src/friend_intro.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces: `pub(crate) fn would_admit(&self, key: K, now_ms: u64) -> bool` on `KeyedSlidingWindow<K>` — `true` iff `key` would be admitted right now, recording nothing. Consumed by Task 2's `OpenJoinRateLimiter::global_has_capacity`.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `src-tauri/src/friend_intro.rs`:

```rust
#[test]
fn keyed_sliding_window_would_admit_does_not_record() {
    let mut w: KeyedSlidingWindow<u64> = KeyedSlidingWindow::new(2, 60_000);
    let k = 7u64;
    let now = 1_000u64;
    // A fresh key is admissible, and peeking never consumes capacity.
    assert!(w.would_admit(k, now));
    assert!(w.would_admit(k, now), "peek must be idempotent — records nothing");
    // Two real admits fill the cap.
    assert!(w.admit(k, now));
    assert!(w.admit(k, now));
    // At cap: both peek and admit report full.
    assert!(!w.would_admit(k, now), "peek reflects a full window");
    assert!(!w.admit(k, now));
    // A zero-cap window never admits, via peek either.
    let z: KeyedSlidingWindow<u64> = KeyedSlidingWindow::new(0, 60_000);
    assert!(!z.would_admit(k, now));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(keyed_sliding_window_would_admit_does_not_record)'`
Expected: FAIL to compile — `no method named would_admit found for struct KeyedSlidingWindow`.

- [ ] **Step 3: Write minimal implementation**

Insert immediately after the closing brace of `fn admit` (before `fn evict`) in `src-tauri/src/friend_intro.rs`:

```rust
    /// `true` if `key` would be admitted right now WITHOUT recording — the
    /// non-mutating companion to `admit` (same `key: K` by-value signature).
    /// For composing two gates where the second must not leave a phantom record
    /// if the first sheds (ZEB-865's aggregate ceiling peeks before the
    /// per-source window records). Counts only in-window (non-stale) entries; a
    /// zero cap never admits.
    pub(crate) fn would_admit(&self, key: K, now_ms: u64) -> bool {
        if self.max == 0 {
            return false;
        }
        let cutoff = now_ms.saturating_sub(self.window_ms);
        match self.windows.get(&key) {
            None => true,
            Some(dq) => dq.iter().filter(|&&t| t >= cutoff).count() < self.max,
        }
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(keyed_sliding_window_would_admit_does_not_record)'`
Expected: PASS.

- [ ] **Step 5: Clippy the touched crate + commit**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5 && cargo fmt --all`
Expected: no warnings; fmt clean.

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && git add src-tauri/src/friend_intro.rs && git commit -m "ZEB-865: add KeyedSlidingWindow::would_admit non-mutating peek

Pure capacity peek (counts non-stale entries < max, records nothing) so a
composed gate can check aggregate capacity before the per-source window records.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D"
```

---

### Task 2: Aggregate ceiling in `OpenJoinRateLimiter` + wire into `verify_and_admit_open_join`

**Files:**
- Modify: `src-tauri/src/open_join_admit.rs` (const near line 41; `OpenJoinReject` enum ~66; `OpenJoinRateLimiter` struct ~109; `new` ~135; methods ~162-193; `verify_and_admit_open_join` step 7 ~376-382)
- Test: `src-tauri/src/open_join_admit.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `KeyedSlidingWindow::would_admit` (Task 1).
- Produces:
  - `pub const OPEN_JOIN_GLOBAL_ADMIT_MAX: usize = 1024;`
  - `OpenJoinReject::NodeCapacity` (unit variant).
  - `pub fn OpenJoinRateLimiter::with_caps(per_source_max: usize, global_max: usize, window_ms: u64) -> Self`
  - `fn admit_source(&mut self, source: [u8; 32], nonce: &[u8; 16], limiter_now_ms: u64) -> Result<(), OpenJoinReject>`

- [ ] **Step 1: Write the failing tests**

Add these to the `#[cfg(test)] mod tests` block in `src-tauri/src/open_join_admit.rs`:

```rust
    /// ZEB-865: the aggregate ceiling caps total admissions across DISTINCT
    /// sources, even ones well within their own per-source budget. Three sources
    /// each admit once (global cap 3 fills), a fourth is shed NodeCapacity.
    #[test]
    fn global_ceiling_bounds_aggregate_across_distinct_sources() {
        let mut rl = OpenJoinRateLimiter::with_caps(20, 3, 60_000);
        let now = 0u64;
        for i in 0..3u8 {
            assert_eq!(
                rl.admit_source([i; 32], &[i; 16], now),
                Ok(()),
                "distinct source {i} within the aggregate ceiling admits"
            );
        }
        assert_eq!(
            rl.admit_source([9u8; 32], &[9u8; 16], now),
            Err(OpenJoinReject::NodeCapacity),
            "aggregate ceiling sheds an under-budget source once the node is full"
        );
    }

    /// ZEB-865: the ceiling must NOT re-create B7's single-source lockout. With a
    /// high global cap, a source exhausting its OWN 20/60 s window is RateLimited
    /// (not NodeCapacity), and a different source still admits.
    #[test]
    fn global_ceiling_does_not_relock_single_source() {
        let mut rl = OpenJoinRateLimiter::with_caps(20, 1024, 60_000);
        let a = [0xAA; 32];
        let now = 0u64;
        for i in 0..20u8 {
            assert_eq!(rl.admit_source(a, &[i; 16], now), Ok(()));
        }
        assert_eq!(
            rl.admit_source(a, &[0xF0; 16], now),
            Err(OpenJoinReject::RateLimited),
            "A over its own budget is RateLimited, not NodeCapacity"
        );
        assert_eq!(
            rl.admit_source([0xBB; 32], &[0xB0; 16], now),
            Ok(()),
            "a different source is unaffected — no cross-source lockout"
        );
    }

    /// ZEB-865 discriminator: a per-source shed must NOT drain the aggregate
    /// ceiling. with_caps(20, 30): source A makes 25 attempts (20 admit + 5
    /// shed). Only the 20 admits spend global tokens, so exactly 10 further
    /// distinct sources admit and the 11th sheds NodeCapacity. A bug where sheds
    /// drained the ceiling would leave only 5 (30 - 25).
    #[test]
    fn single_source_shed_does_not_drain_global_ceiling() {
        let mut rl = OpenJoinRateLimiter::with_caps(20, 30, 60_000);
        let a = [0xAA; 32];
        let now = 0u64;
        for i in 0..20u8 {
            assert_eq!(rl.admit_source(a, &[i; 16], now), Ok(()));
        }
        for i in 20..25u8 {
            assert_eq!(
                rl.admit_source(a, &[i; 16], now),
                Err(OpenJoinReject::RateLimited),
                "A's over-budget attempts are per-source shed"
            );
        }
        for i in 0..10u8 {
            assert_eq!(
                rl.admit_source([0x40 + i; 32], &[0x80 + i; 16], now),
                Ok(()),
                "distinct source {i} fits the remaining aggregate headroom (30 - 20)"
            );
        }
        assert_eq!(
            rl.admit_source([0x50; 32], &[0x90; 16], now),
            Err(OpenJoinReject::NodeCapacity),
            "11th further source exceeds the ceiling — the 5 sheds spent no tokens"
        );
    }

    /// ZEB-865: the aggregate window rolls on the limiter's OWN monotonic clock,
    /// like the per-source window. Fill it at t=0, advance past the window, admits
    /// resume.
    #[test]
    fn global_ceiling_keys_on_monotonic_clock() {
        let mut rl = OpenJoinRateLimiter::with_caps(20, 2, 60_000);
        for i in 0..2u8 {
            assert_eq!(rl.admit_source([i; 32], &[i; 16], 0), Ok(()));
        }
        assert_eq!(
            rl.admit_source([9; 32], &[9; 16], 0),
            Err(OpenJoinReject::NodeCapacity),
            "ceiling full at t=0"
        );
        assert_eq!(
            rl.admit_source([9; 32], &[0x19; 16], OPEN_JOIN_RATE_LIMIT_WINDOW_MS + 1),
            Ok(()),
            "aggregate window rolled on the monotonic limiter clock"
        );
    }

    /// ZEB-865: a request shed by the aggregate ceiling must not persist its
    /// nonce (it was never accepted) — through the real gate, the SAME nonce
    /// admits after the window rolls.
    #[test]
    fn globally_shed_request_nonce_is_retryable_after_window() {
        let f = Fixture::new();
        let mut lim = OpenJoinRateLimiter::with_caps(
            OPEN_JOIN_RATE_LIMIT_PER_WINDOW,
            1, // global cap 1 → the second distinct source is ceiling-shed
            OPEN_JOIN_RATE_LIMIT_WINDOW_MS,
        );
        let src_a = [0x01; 32];
        let src_b = [0x02; 32];

        // First request fills the aggregate ceiling (global 1/1).
        let (req0, sig0, sb0) = f.fresh_request();
        verify_and_admit_open_join(
            &req0, &sig0, &sb0, &f.epoch_key, f.community_id, f.admin_addr,
            &f.current_events, f.now_ms, FRESHNESS, f.now_ms, src_a, &mut lim,
        )
        .expect("first request admits and fills the ceiling");

        // Second request (different source, fixed nonce [0x07;16]) is ceiling-shed.
        let (req1, sig1, sb1) = f.valid_request();
        assert_eq!(
            verify_and_admit_open_join(
                &req1, &sig1, &sb1, &f.epoch_key, f.community_id, f.admin_addr,
                &f.current_events, f.now_ms, FRESHNESS, f.now_ms, src_b, &mut lim,
            )
            .unwrap_err(),
            OpenJoinReject::NodeCapacity,
        );

        // After the window rolls, the SAME nonce admits (never persisted).
        let later = f.now_ms + OPEN_JOIN_RATE_LIMIT_WINDOW_MS + 1;
        verify_and_admit_open_join(
            &req1, &sig1, &sb1, &f.epoch_key, f.community_id, f.admin_addr,
            &f.current_events, later, OPEN_JOIN_RATE_LIMIT_WINDOW_MS * 4, later, src_b, &mut lim,
        )
        .expect("a ceiling-shed nonce is admissible after the window");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(open_join_admit)'`
Expected: FAIL to compile — `no variant NodeCapacity`, `no function with_caps`, `no method admit_source`. (`test(open_join_admit)` matches every test in the `open_join_admit::tests` module by path prefix — new and existing.)

- [ ] **Step 3a: Add the const**

In `src-tauri/src/open_join_admit.rs`, immediately after the `OPEN_JOIN_RATE_LIMIT_WINDOW_MS` const (line 41):

```rust
/// ZEB-865: node-wide aggregate admissions accepted per
/// [`OPEN_JOIN_RATE_LIMIT_WINDOW_MS`] before excess is shed as
/// [`OpenJoinReject::NodeCapacity`]. 1024 = 51× the per-source budget: far above
/// any realistic single-beacon honest burst (joiners also spread across the
/// butler set and retry on shed), while cutting the uncapped Sybil worst case
/// (`MAX_WINDOW_KEYS × OPEN_JOIN_RATE_LIMIT_PER_WINDOW` ≈ 163,840/60 s) ~160×.
/// Defense-in-depth atop the per-source admission budget and the Tier-1
/// connection shield, so it is sized to favor never locking out honest load.
pub const OPEN_JOIN_GLOBAL_ADMIT_MAX: usize = 1024;
```

- [ ] **Step 3b: Add the reject variant**

In the `OpenJoinReject` enum, immediately after the `RateLimited` variant (line 91):

```rust
    /// Node-wide aggregate admission ceiling exceeded (ZEB-865). Distinct from
    /// `RateLimited` (per-source): the source is within its own budget but the
    /// node is at aggregate capacity. Same benign typed-rejection wire behavior
    /// as the other post-decode rejects.
    NodeCapacity,
```

- [ ] **Step 3c: Add the `global` field**

In the `OpenJoinRateLimiter` struct, immediately after the `windows` field (line 117):

```rust
    /// ZEB-853 (B7): per-source admission windows ... (existing doc unchanged)
    windows: KeyedSlidingWindow<[u8; 32]>,
    /// ZEB-865: node-wide aggregate admission ceiling, checked in ADDITION to
    /// the per-source `windows`. A single unit-key reuse of the audited
    /// sliding-window primitive — exactly one global 60 s window (the
    /// `MAX_WINDOW_KEYS` eviction is a no-op at one key). The per-source budget
    /// alone can't bound a Sybil fan-out (each fake source gets its own window);
    /// this aggregate gate caps the sum.
    global: KeyedSlidingWindow<()>,
```

(Only the `global` field is new — the `windows` line already exists; add the new field right after it.)

- [ ] **Step 3d: Rewrite `new` to delegate + add `with_caps`**

Replace the existing `new` (lines 134-145):

```rust
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            windows: KeyedSlidingWindow::new(
                OPEN_JOIN_RATE_LIMIT_PER_WINDOW,
                OPEN_JOIN_RATE_LIMIT_WINDOW_MS,
            ),
            seen_nonces: HashSet::new(),
            nonce_seen_at: HashMap::new(),
            epoch: tokio::time::Instant::now(),
        }
    }
```

with:

```rust
    /// Fresh limiter with production caps, its monotonic epoch anchored now.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self::with_caps(
            OPEN_JOIN_RATE_LIMIT_PER_WINDOW,
            OPEN_JOIN_GLOBAL_ADMIT_MAX,
            OPEN_JOIN_RATE_LIMIT_WINDOW_MS,
        )
    }

    /// Test/tuning constructor — deterministic tiny caps for the per-source and
    /// aggregate windows (mirrors [`OpenJoinConnLimiter::with_caps`]).
    pub fn with_caps(per_source_max: usize, global_max: usize, window_ms: u64) -> Self {
        Self {
            windows: KeyedSlidingWindow::new(per_source_max, window_ms),
            global: KeyedSlidingWindow::new(global_max, window_ms),
            seen_nonces: HashSet::new(),
            nonce_seen_at: HashMap::new(),
            epoch: tokio::time::Instant::now(),
        }
    }
```

- [ ] **Step 3e: Add the aggregate methods + `admit_source`**

Immediately after the existing `record_nonce` method (line 193):

```rust
    /// ZEB-865: node-wide aggregate capacity peek (no record). Composed BEFORE
    /// the per-source `allow` so a ceiling shed charges neither the source's
    /// budget nor its nonce.
    fn global_has_capacity(&self, limiter_now_ms: u64) -> bool {
        self.global.would_admit((), limiter_now_ms)
    }

    /// ZEB-865: commit one aggregate token. Called ONLY after `allow` admits, so
    /// a per-source shed never drains the ceiling (which would let one spammer
    /// re-create single-source lockout).
    fn record_global(&mut self, limiter_now_ms: u64) {
        self.global.admit((), limiter_now_ms);
    }

    /// ZEB-865: the whole rate-limit decision for one open-join request — replay
    /// + node-wide aggregate ceiling + per-source budget + nonce record, in the
    /// one order that keeps a ceiling shed from charging the source's budget or
    /// nonce, and keeps a per-source shed from draining the aggregate ceiling.
    /// `verify_and_admit_open_join` and the unit tests share this one ordering.
    fn admit_source(
        &mut self,
        source: [u8; 32],
        nonce: &[u8; 16],
        limiter_now_ms: u64,
    ) -> Result<(), OpenJoinReject> {
        if self.is_replay(nonce, limiter_now_ms) {
            return Err(OpenJoinReject::Replay);
        }
        if !self.global_has_capacity(limiter_now_ms) {
            return Err(OpenJoinReject::NodeCapacity);
        }
        if !self.allow(source, limiter_now_ms) {
            return Err(OpenJoinReject::RateLimited);
        }
        self.record_global(limiter_now_ms);
        self.record_nonce(nonce, limiter_now_ms);
        Ok(())
    }
```

- [ ] **Step 3f: Rewire `verify_and_admit_open_join` step 7**

Replace the current step-7 block (lines 370-382 — the comment plus the `is_replay` / `allow` / `record_nonce` trio):

```rust
    // 7. Replay + rate-limit (after cheap structural + crypto checks, before the
    //    stateful materialization). Replay is CHECKED first (without recording)
    //    so a replayed nonce is reported as Replay rather than masked by a
    //    coincident rate-limit shed. The nonce is RECORDED only AFTER the rate
    //    limit also passes — otherwise a `RateLimited` rejection would persist
    //    the nonce and a legitimate retry would be wrongly rejected as a replay.
    if limiter.is_replay(&req.nonce, limiter_now_ms) {
        return Err(OpenJoinReject::Replay);
    }
    if !limiter.allow(source_id, limiter_now_ms) {
        return Err(OpenJoinReject::RateLimited);
    }
    limiter.record_nonce(&req.nonce, limiter_now_ms);
```

with:

```rust
    // 7. Replay + per-source budget + node-wide aggregate ceiling + nonce record,
    //    all in one ordering owned by `admit_source` (ZEB-865). Runs after the
    //    cheap structural + crypto checks, before the stateful materialization —
    //    so the aggregate ceiling (which only fully-verified requests can reach)
    //    sheds the dominant materialization cost of a Sybil fan-out. A ceiling
    //    shed charges neither the source's budget nor its nonce (cleanly
    //    retryable), and a per-source shed never drains the aggregate ceiling.
    limiter.admit_source(source_id, &req.nonce, limiter_now_ms)?;
```

- [ ] **Step 4: Run the new + existing module tests to verify they pass**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(open_join_admit)'`
Expected: PASS — the whole `open_join_admit::tests` module: the 5 new tests plus every pre-existing test (per-source paths byte-identical).

- [ ] **Step 5: Clippy `--all-targets` + fmt + commit**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5 && cargo fmt --all -- --check`
Expected: no warnings; fmt clean. (If fmt reports diffs, run `cargo fmt --all` and re-stage.)

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && git add src-tauri/src/open_join_admit.rs && git commit -m "ZEB-865: node-wide aggregate open-join admission ceiling (1024/60s)

Adds a single-key global sliding window to OpenJoinRateLimiter, composed in a
new admit_source() decision: replay -> aggregate peek -> per-source allow ->
record global -> record nonce. Peek-before/record-after ordering means a ceiling
shed charges neither the source's budget nor its nonce, and a per-source shed
never drains the ceiling. Reached only by fully-verified requests, so it can't
be cheaply exhausted into the pre-B7 global lockout. Acceptor untouched.

Closes ZEB-865.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D"
```

---

## Final verification (before opening the PR)

Run the full CI-parity sweep from `src-tauri/` (NOT test-select — this is the pre-PR gate):

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && \
  cargo fmt --all -- --check && \
  cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && \
  cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: fmt clean, clippy clean, full suite green (the prior full suite was 5620 passed / 6 skipped; this adds 6 tests → ~5626). Capture the pass/skip counts for the PR body.

## Spec coverage self-check

| Spec element | Task |
|---|---|
| `global: KeyedSlidingWindow<()>` field | 2 (3c) |
| `OPEN_JOIN_GLOBAL_ADMIT_MAX = 1024` | 2 (3a) |
| `KeyedSlidingWindow::would_admit` peek | 1 |
| `with_caps` constructor + `new` delegation | 2 (3d) |
| `global_has_capacity` / `record_global` | 2 (3e) |
| `NodeCapacity` reject variant | 2 (3b) |
| `admit_source` composite ordering | 2 (3e) |
| step-7 rewire in `verify_and_admit_open_join` | 2 (3f) |
| aggregate bound test | 2 (test 1) |
| anti-lockout preserved test | 2 (test 2) |
| no-drain discriminator test | 2 (test 3) |
| monotonic-clock test | 2 (test 4) |
| globally-shed nonce retryable test | 2 (test 5) |
| would_admit-doesn't-record test | 1 |
| acceptor untouched | (no task modifies `iroh_invite_acceptor.rs`) |
