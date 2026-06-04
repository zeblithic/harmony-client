# ZEB-347: Stabilize iroh/zenoh transport-test flakes — Design

**Status:** Approved (2026-06-03)
**Ticket:** [ZEB-347](https://linear.app/zeblith/issue/ZEB-347)
**Branch:** `zeb-347-stabilize-iroh-zenoh-transport-test-flakes` (off `origin/main` `9b0e532`)
**Type:** Test-infrastructure fix (no production code behavior change)

---

## Problem

Six integration/unit tests time out intermittently ("flaky") under the CI `rust-test`
job's full-workspace `cargo nextest run --workspace --all-targets`. They reliably pass
on rerun (`gh run rerun --failed`) and in lighter local runs. They have cost us a manual
`--failed` rerun on essentially every voice PR's convergence step.

The six:

| # | Test | File | Outer budget | Runtime flavor |
|---|------|------|--------------|----------------|
| 1 | `reachability_publisher::tests::force_notify_triggers_publish` | `src/reachability_publisher.rs` | 30s | `#[tokio::test]` |
| 2 | `zeb_321_connectivity_ipc_tests::force_republish_wakes_publisher` | `src/lib.rs` | 60s | `#[tokio::test]` |
| 3 | `zenoh_iroh_link::tests::paired_stream_roundtrip_via_loopback` | `src/zenoh_iroh_link.rs` | 30s | `#[tokio::test]` |
| 4 | `zenoh_iroh_transport::tests::drain_dispatches_queued_connections_in_parallel` | `src/zenoh_iroh_transport.rs` | 45s | `#[tokio::test(multi_thread, worker_threads = 4)]` |
| 5 | `zenoh_iroh_transport::tests::handshake_connection_queued_pre_install_dispatched_on_install` | `src/zenoh_iroh_transport.rs` | 45s | `#[tokio::test(multi_thread, worker_threads = 4)]` |
| 6 | `community_reachability_two_engine_integration::two_engines_exchange_via_iroh_zenoh` | `tests/community_reachability_two_engine_integration.rs` | 30s | `#[tokio::test]` |

---

## Root cause (Phase 1 investigation — confirmed)

**Load-induced CPU starvation of real QUIC loopback I/O. Not a product bug.**

1. **All six perform real iroh QUIC loopback binds + handshakes.** A `build_hermetic_iroh_endpoint()`
   bind is ~10s solo; under the full `--workspace --all-targets` suite (~80 test binaries
   *linking and executing concurrently* on a 2-vCPU `ubuntu-latest`), observed binds balloon
   to ~20s+. The `tokio::time::timeout(...)` calls in these tests are **"don't-hang-forever"
   regression guards, never latency assertions** — but they lose the race against the
   starved runtime.

2. **The two notify-path tests (1 & 2) were the prime real-bug suspects, and are clean.**
   `ReachabilityPublisher` waits on a `tokio::sync::Notify` inside a `biased` `select!` that
   constructs a **fresh `notified()` future every loop iteration**. There is no
   condition-check-then-await gap, so a `notify_one()` that arrives mid-publish is stored as a
   permit and consumed on the next poll — **no lost-wakeup is possible.** The queue/dispatch
   tests (4 & 5) exercise a `TokioMutex` + `OnceCell` + per-connection `tokio::spawn` path that
   is atomic and race-free (the parallelism it asserts is the *fix* from ZEB-325 PR #159 R3-3).

3. **Thread oversubscription multiplier.** Tests 4 & 5 each spin up their own
   `worker_threads = 4` multi-threaded runtime, so when several heavy tests overlap, the real
   OS-thread demand far exceeds the 2 vCPUs.

4. **Nothing in the config controls concurrency of the heavy tests.** `src-tauri/.config/nextest.toml`
   has **no `[test-groups]`, no `threads-required`, no `retries`.** The tests run at full fan-out
   in the main 30-min `rust-test` job.

5. **Prior mitigation was a timeout-bump arms race.** Git history shows two reactive
   budget-bump commits (`c089127` 15s→30s on test 3; `9cade27` on test 2). Bumping the clock
   moves the threshold without addressing oversubscription, so the flakes recur. The variable
   that actually changes between "passes" and "flakes" is *how many real-QUIC tests run
   simultaneously* — and that was uncontrolled.

---

## Approved fix

**Serialize + right-size + bounded-retry safety net.** Stop chasing the clock; cap the
concurrency of the resource-heavy tests, trim their thread footprint, and absorb irreducible
network variance with scoped retries.

### 1. Serialize the heavy network tests (nextest `test-group`)

Add to `src-tauri/.config/nextest.toml`:

```toml
[test-groups]
# Real iroh/zenoh QUIC loopback tests. Each does multi-second QUIC binds +
# handshakes; running several at once on a 2-vCPU CI runner starves them into
# their wall-clock "don't-hang" timeouts (ZEB-347). Cap to one-at-a-time so a
# heavyweight network test gets the CPU it needs relative to the cheap unit
# tests, instead of competing with its siblings. max-threads is tunable to 2 if
# the serialized wall-clock cost ever matters; 1 is the conservative default.
network-loopback = { max-threads = 1 }

[[profile.default.overrides]]
filter = """\
  test(force_notify_triggers_publish) \
  + test(force_republish_wakes_publisher) \
  + test(paired_stream_roundtrip_via_loopback) \
  + test(drain_dispatches_queued_connections_in_parallel) \
  + test(handshake_connection_queued_pre_install_dispatched_on_install) \
  + test(two_engines_exchange_via_iroh_zenoh)\
  """
test-group = "network-loopback"
# Bounded safety net for irreducible real-network-I/O variance under shared CI.
# SCOPED to these six only — we never want to mask a flake in an unrelated test
# (that would hide a real bug; "test drift is our fault"). 2 retries = 3 attempts.
retries = 2
```

- The `test(...)` substring filters are unambiguous (each name is long and unique).
- The override composes cleanly with the existing `default-filter` (which only *excludes*
  17 unrelated pre-existing-broken tests): `default-filter` decides what runs; the override
  decides group + retry for what runs.

### 2. Right-size the thread-greedy tests (`worker_threads` 4 → 2)

In `src/zenoh_iroh_transport.rs`, change tests 4 & 5 from
`#[tokio::test(flavor = "multi_thread", worker_threads = 4)]` to `worker_threads = 2`.
Their parallelism assertion (a gated first dispatch must not block the second) needs only
**≥2** worker threads — 4 is wasteful thread pressure. **This is the one change with logic
risk and MUST be verified under load (see Verification).**

### 3. No timeout-budget changes

We deliberately do **not** bump any `tokio::time::timeout` budget further — that is the
arms-race anti-pattern this fix replaces. Budgets stay as-is; serialization makes them
comfortably sufficient.

---

## Risks & mitigations

| Risk | Mitigation |
|------|-----------|
| `worker_threads = 2` regresses test 4's gated-parallelism logic (real risk) | Loop tests 4 & 5 ~20× locally under `-j` oversubscription before/after; revert to 4 for that test if any failure (keep the test-group serialization, which is the dominant fix). |
| `test(...)` filter accidentally matches an unintended test | Names are long + unique; verify with `cargo nextest list --features test-fixtures -E '<filter>'` shows exactly the six. |
| Serialization adds too much CI wall-clock | These tests overlap with the ~2000 cheap parallel tests, so net add is bounded (~1–2 min). `max-threads` is tunable to 2 if needed. The `rust-test` job has a 30-min budget vs. ~12-min current. |
| Retries mask a *real* future regression in one of the six | Retries are scoped to these six known-load-sensitive tests only; a genuine deterministic break still fails all 3 attempts. nextest reports retried passes as "flaky" (visible signal), not silently green. |

---

## Verification

This is a flake fix, so verification is empirical (no single deterministic failing test):

1. **Filter correctness:** `cargo nextest list --features test-fixtures -E '<the override filter>'`
   lists **exactly** the six tests.
2. **`worker_threads = 2` safety (the logic-risk gate):** run tests 4 & 5 under deliberate
   oversubscription, repeated, e.g.
   `for i in $(seq 1 20); do cargo nextest run --features test-fixtures -j 16 -E 'test(drain_dispatches_queued_connections_in_parallel) + test(handshake_connection_queued_pre_install_dispatched_on_install)' || break; done`
   — must be 20/20 green.
3. **Group serialization honored:** confirm the six no longer overlap (nextest run output;
   group cap observed) and pass a few full-suite `cargo nextest run --workspace --all-targets`
   runs locally.
4. **Standard gates:** `cargo fmt --all -- --check`, `cargo clippy --all-targets --features
   test-fixtures -- -D warnings`, full `cargo nextest run --workspace --all-targets --features
   test-fixtures` green except (ideally now including) the six. tsc/vitest unaffected
   (no frontend change) but run as a guard in the final sweep.
5. **CI is the real proof:** the `rust-test` job greens on the PR without a `--failed` rerun,
   ideally across the bot-review-loop's multiple CI runs.

---

## Non-goals

- **No production code changes.** The notify/queue/dispatch primitives are race-free; we are
  not "fixing" them.
- **No further timeout-budget inflation** (the anti-pattern being retired).
- **Not mocking iroh / removing real QUIC** — these are integration tests whose value is
  proving real transport works; we keep them real, just well-scheduled.
- **Not touching the 17 unrelated `default-filter`-excluded tests** (tracked separately in
  ZEB-332).

---

## Files touched

- `src-tauri/.config/nextest.toml` — add `[test-groups]` + `[[profile.default.overrides]]` (serialize + retry).
- `src-tauri/src/zenoh_iroh_transport.rs` — `worker_threads = 4` → `2` on tests 4 & 5.
- `docs/specs/2026-06-03-zeb-347-stabilize-iroh-zenoh-transport-test-flakes-design.md` — this doc.
- `docs/plans/2026-06-03-zeb-347-stabilize-iroh-zenoh-transport-test-flakes-plan.md` — the plan.

Scope: ~40 lines across two source files + config. Verification-dominated; executed directly
with rigorous local load-testing rather than the full subagent ceremony (proportionate to a
config-level change), then the standard PR + autonomous bot-review loop.
