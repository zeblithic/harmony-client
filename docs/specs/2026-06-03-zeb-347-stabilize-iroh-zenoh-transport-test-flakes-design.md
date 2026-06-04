# ZEB-347: Stabilize iroh/zenoh transport-test flakes — Design

**Status:** Approved (2026-06-03)
**Ticket:** [ZEB-347](https://linear.app/zeblith/issue/ZEB-347)
**Branch:** `zeb-347-stabilize-iroh-zenoh-transport-test-flakes` (off `origin/main` `9b0e532`)
**Type:** Test-infrastructure fix (no production code change)

> **Note on revision:** an earlier draft of this spec attributed the flakes to nextest
> oversubscription and proposed a test-group + retry + `worker_threads` fix. Implementation-time
> investigation (systematic-debugging) **overturned that root cause** — see below. This document
> reflects the corrected diagnosis and the fix that was actually built and verified.

---

## Problem

Six integration/unit tests time out intermittently under the CI `rust-test` job's full-workspace
`cargo nextest run --workspace --all-targets`. They reliably pass on rerun (`gh run rerun --failed`)
and have cost a manual `--failed` rerun on essentially every voice PR's convergence step.

| # | Test | File | Outer budget |
|---|------|------|--------------|
| 1 | `reachability_publisher::tests::force_notify_triggers_publish` | `src/reachability_publisher.rs` | 30s |
| 2 | `zeb_321_connectivity_ipc_tests::force_republish_wakes_publisher` | `src/lib.rs` | 60s |
| 3 | `zenoh_iroh_link::tests::paired_stream_roundtrip_via_loopback` | `src/zenoh_iroh_link.rs` | 30s |
| 4 | `zenoh_iroh_transport::tests::drain_dispatches_queued_connections_in_parallel` | `src/zenoh_iroh_transport.rs` | 45s |
| 5 | `zenoh_iroh_transport::tests::handshake_connection_queued_pre_install_dispatched_on_install` | `src/zenoh_iroh_transport.rs` | 45s |
| 6 | `community_reachability_two_engine_integration::two_engines_exchange_via_iroh_zenoh` | `tests/community_reachability_two_engine_integration.rs` | 30s |

---

## Root cause (corrected, empirically confirmed)

**The first `iroh::Endpoint::bind()` in a process triggers a one-time, process-global
initialization that is slow and environment-dependent (~10s on CI, ~31s on the dev Mac it was
characterized on). Every *subsequent* bind in the same process is ~3ms.**

Proof (single process, two binds, timed):

```
ZEB347_MINIMAL_BARE_BIND = 30.275 s   // first bind (presets::Minimal, relay disabled)
ZEB347_BIND_ELAPSED      =  3.238 ms  // second bind, same process (presets::N0)
```

The init is **process-global**, not preset-specific. Because **`cargo nextest` runs each test in
its own process**, every one of the six tests is a "first bind" and pays the full init. The tests
wrap that bind inside a tight `tokio::time::timeout`, so when the one-time init lands near or above
the budget (the ~10s CI floor balloons under parallel CPU starvation), the timeout fires → flake.

**Ruled out empirically** (each tested, none was the cause): relay (`RelayMode::Disabled` already
set), portmapper (`PortmapperConfig::Disabled` made no difference), the N0-vs-Minimal preset
(address-lookup — first bind is slow for *both*), and CA-root/keychain loading (the default
`CaRootsConfig` is `Mode::EmbeddedWebPki`, compiled-in, no system read). The exact identity of the
global init was not pinned (it emits no iroh trace during the wait, and macOS blocks process
sampling here) — but its *shape* is certain and is all the fix depends on.

The earlier "oversubscription" framing was a symptom, not the cause: oversubscription *amplifies*
the one-time init (CPU starvation stretches the ~10s past the budget), but the init is paid once
per process regardless of parallelism.

---

## Design insight → the fix

The deeper issue is a **test-design** one. These `tokio::time::timeout` wrappers exist to catch a
**hung behavior** — a lost wakeup, a deadlocked QUIC teardown. They were never meant to police how
long hermetic *setup* (the bind) takes. Folding a slow, one-time, environment-variable init into an
asserted timeout is what makes the tests flaky, for zero real signal.

**Fix: hoist the one-time bind init out of the asserted region via a warm-up.**

Add a shared test helper that performs one throwaway hermetic bind to prime the process-global
init, then call it once at the top of each of the six tests — **before** their `timeout` wrapper.
After the warm-up returns, each test's own `bind()` is the fast ~3ms cached path, so the asserted
region is fast and budget-safe on every machine and under any load.

```rust
// src/iroh_endpoint.rs (module level, feature-gated)
#[cfg(any(test, feature = "test-fixtures"))]
#[allow(dead_code)]
pub async fn warm_up_iroh_global_init() {
    let ep = Endpoint::builder(presets::Minimal)
        .relay_mode(iroh::endpoint::RelayMode::Disabled)
        .clear_ip_transports()
        .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("warm-up bind_addr loopback")
        .bind().await.expect("warm-up iroh bind");
    ep.close().await;
}
```

Each of the six tests gains one line before its timeout:

```rust
crate::iroh_endpoint::warm_up_iroh_global_init().await;   // (harmony_app:: in the integration test)
tokio::time::timeout(Duration::from_secs(N), inner()).await.expect(...);
```

**No wall-clock regression:** the one-time init is paid exactly once per process either way — the
warm-up just relocates it *outside* the assertion. Total per-test wall-clock is ~the same as today
(~10s CI / ~31s dev Mac), but the asserted region no longer trips.

### What this fix deliberately does NOT include

- **No nextest `test-group` / serialization** — unnecessary; the warm-up makes the asserted region
  fast regardless of parallelism.
- **No retries** — nothing to retry; the tests pass deterministically.
- **No timeout-budget changes** — the budgets are fine once the init is out of the asserted region.
- **No `worker_threads` change** — irrelevant once the binds inside the assertions are ~3ms.

---

## Verification (done)

- `force_notify` alone: was FAIL@31s → **PASS** after warm-up.
- All six in parallel (the exact scenario where all six previously failed): **6/6 PASS** locally on
  the dev Mac (flagged "slow" because the *warm-up* balloons to ~76s under 6× contention on this
  Mac — a warning, not a failure; on CI the warm-up is ~10s and they won't be slow).
- `cargo fmt --all -- --check` clean; `cargo clippy -p harmony-app --lib --features test-fixtures
  -- -D warnings` clean.
- The change is **test-only** (a feature-gated helper + six one-line calls), so it cannot affect
  non-test code; the full `--workspace --all-targets` nextest is left to CI, where iroh binds are
  ~10s and the suite is reliable (a full local sweep on this Mac is dominated by unrelated iroh
  tests' slow binds).
- **CI is the durable proof:** the `rust-test` job should green without a `--failed` rerun across
  the PR's CI runs.

---

## Non-goals / follow-ups

- **Pinning & eliminating the global init itself** (to make first-bind fast everywhere, restoring a
  fast full *local* sweep) is a separate, uncertain-scope investigation — out of scope here.
- **Other iroh-binding tests not in the reported flake set** (`pkarr_iroh_redeem_full_integration`,
  `network_health_two_endpoint`) have 60s budgets with ample CI headroom and are not warmed up here;
  the same one-line helper applies if any of them is observed to flake.
- No production code change; no frontend change.

---

## Files touched

- `src/iroh_endpoint.rs` — add `warm_up_iroh_global_init()` helper.
- `src/reachability_publisher.rs`, `src/lib.rs`, `src/zenoh_iroh_link.rs`,
  `src/zenoh_iroh_transport.rs` (×2), `tests/community_reachability_two_engine_integration.rs` —
  one warm-up call at the top of each of the six tests.
- `docs/specs/…-design.md` / `docs/plans/…-plan.md` — this doc + the plan.
