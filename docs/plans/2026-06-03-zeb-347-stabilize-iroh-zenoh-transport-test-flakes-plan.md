# ZEB-347 Stabilize iroh/zenoh transport-test flakes — Implementation Plan

> Verification-dominated, test-only change. Executed directly by the controller with local
> load-testing on the dev Mac (the harshest environment for this bug), then PR + autonomous
> bot-review loop. Spec: `docs/specs/2026-06-03-zeb-347-…-design.md`.

**Goal:** Make the six real-QUIC iroh/zenoh transport tests deterministic by hoisting the one-time,
process-global iroh bind init out of each test's asserted `tokio::time::timeout` via a warm-up,
so the bind inside the assertion is the fast ~3ms cached path.

**Architecture:** Root cause = a one-time process-global init on the FIRST `Endpoint::bind()` per
process (~10s CI / ~31s dev Mac; nextest is process-per-test so every test pays it). Fix lives
entirely in test code: one feature-gated helper + six one-line calls. No production change, no
serialize/retry/budget/`worker_threads` change.

---

### Task 1: Warm-up helper

**Files:** `src/iroh_endpoint.rs`

- [x] Add module-level `#[cfg(any(test, feature = "test-fixtures"))] #[allow(dead_code)] pub async
  fn warm_up_iroh_global_init()` that binds one throwaway hermetic endpoint (`presets::Minimal`,
  `RelayMode::Disabled`, `clear_ip_transports`, loopback) and `.close()`s it. `pub` + feature-gated
  so the integration test (public `--features test-fixtures` surface) can call it; `#[allow(dead_code)]`
  because the non-test lib target never calls it. Doc-comment explains the why (timeout guards hung
  behavior, not slow setup).

---

### Task 2: Wire the warm-up into all six tests

**Files:** `src/reachability_publisher.rs`, `src/lib.rs`, `src/zenoh_iroh_link.rs`,
`src/zenoh_iroh_transport.rs` (×2), `tests/community_reachability_two_engine_integration.rs`

- [x] Insert `crate::iroh_endpoint::warm_up_iroh_global_init().await;` (─ `harmony_app::…` in the
  integration test) as the first statement of each of the six test fns — **before** the
  `tokio::time::timeout(...)` wrapper — with a ZEB-347 comment. No other change to the tests
  (budgets, `worker_threads`, bodies all untouched).

---

### Task 3: Verify + gate + ship

- [x] **`force_notify` alone:** FAIL@31s → PASS after warm-up (proves the mechanism).
- [x] **All six in parallel** (the scenario where all six previously failed): 6/6 PASS on the dev
  Mac (flagged "slow" only because the warm-up balloons under 6× contention here; ~10s on CI).
- [x] `cargo fmt --all -- --check` clean; `cargo clippy -p harmony-app --lib --features
  test-fixtures --locked -- -D warnings` clean.
- [ ] Commit (helper + six call sites + docs), push, open PR.
- [ ] **Autonomous bot-review loop.** Win condition: the `rust-test` CI job greens **without** a
  `--failed` rerun across the loop's CI runs. NEVER trigger Greptile. Do NOT self-merge — Jake's
  gate. Pushover at ready-to-merge.

---

## Done = the six pass deterministically

- All six PASS in parallel locally (done).
- `rust-test` greens on the PR with no manual rerun (CI — the durable proof).
- No production/frontend change; full `--workspace --all-targets` nextest verified by CI.
