# ZEB-385 — Real Network Health self-test

**Status:** Design approved 2026-06-08
**Issue:** [ZEB-385](https://linear.app/zeblith/issue/ZEB-385) (Medium, Bug) — *Network Health self-test still synthetic (all steps "skipped") + cross-wan-validation.md wrongly tells testers to "expect ✓"*
**Branch:** `zeb-385-real-network-health-self-test` (off `origin/main` `ec70764f`)
**Scope:** harmony-client only. No upstream `harmony-pkarr` change. Backend Rust + one doc.

---

## 1. Problem

The in-app Network Health self-test returns every step as `⊘ skipped` — the Phase-1 synthetic stub in `network_health_run_self_test` (`src-tauri/src/lib.rs:37102`). Meanwhile `docs/cross-wan-validation.md` Step 1 tells testers "Click Run self-test. Expect every step (endpoint, relay, pkarr_publish, pkarr_resolve) to show ✓." A perfectly healthy node therefore looks broken (confirmed live on Koya 2026-06-06: all four steps `⊘`). Before external alpha testers run the playbook, this would generate a flood of false bug reports.

This is the honest-observability sequel to ZEB-379 (which made the desktop app emit logs at all). ZEB-379 made the app observable to *us*; ZEB-385 makes the app's *self-reported* health honest to *testers*.

## 2. What already exists (the seam)

ZEB-329 built a complete, unit-tested self-test seam in `src-tauri/src/network_health.rs`:

- Three traits:
  - `IrohSelfTest { fn endpoint_bound(&self) -> bool; fn relay_round_trip(&self) -> BoxFuture<Result<Duration, String>> }`
  - `PkarrSelfTest { fn publish_identity(&self) -> BoxFuture<Result<Duration, String>>; fn resolve_self(&self) -> BoxFuture<Result<Duration, String>> }`
  - `PingDispatcher { fn ping(node_id_bytes, timeout) -> BoxFuture<Result<(Duration, ConnectionMode), String>> }`
- An orchestrator `NetworkHealthService::run_self_test(iroh, pkarr, ping)` (`network_health.rs:1023`) that runs the four ordered local steps with the §6.2 cascade (a downstream step is `Skipped`, not `Failed`, when an upstream step did not pass) plus semaphore-bounded per-peer pings. It caches the report for `network_health_export_payload`.
- A `NullDispatcher` stub for the deferred ping side.
- Wire types `SelfTestReport { started_at_ms, finished_at_ms, steps: Vec<SelfTestStep>, peer_results }`, `SelfTestStep { name, outcome: StepOutcome }`, `enum StepOutcome { Pass { duration_ms }, Fail { reason }, Skipped { reason } }`, mirrored in `src/lib/types/network-health.ts`.

**The work is not new infrastructure.** It is: implement production trait impls, flip the IPC to call `run_self_test` instead of fabricating a synthetic, and a small return-type refactor of the orchestrator so a probe can self-skip.

## 3. Confirmed primitives (all reachable client-side)

| Need | Primitive | Location |
|---|---|---|
| endpoint bound | `IrohEndpoint::node_id()` (via `NodeState.iroh_endpoint: Option<Arc<IrohEndpoint>>`) | `lib.rs:848`, `iroh_endpoint.rs` |
| relay round-trip | `PkarrResolver::resolve(&VerifyingKey) -> Result<Option<_>, String>` (public) | `harmony_pkarr` resolver |
| resolve self | `PkarrResolver::resolve_window(&[VerifyingKey])` + `epoch_tolerance_window` + `derive_ephemeral_key(PkarrCase::Identity, &id_pub, &epoch)` + `PkarrRoutingRecord::{verify_inner_sig, verify_identity_match, verify_skew}` | `harmony_pkarr` (all public; same recipe as `connectivity_discover_identity`, `lib.rs:34317`) |
| identity published? | `PkarrPublisher::active_handles() -> Vec<String>` contains `"identity"` | `harmony_pkarr` publisher |
| discoverable? | `PkarrSettings::identity_discoverable` (persisted; `NodeState.pkarr_identity_publisher`/settings path) | `lib.rs:4675`, `pkarr_settings.rs` |
| own identity pub | `NodeState.dm_identity_pub_64: Option<[u8; 64]>` | `lib.rs:633` |

**Two facts that shape the design:**

1. **Identity publish is opt-in.** At boot, `if pkarr_settings.identity_discoverable { pkarr_identity_pub.enable().await }` (`lib.rs:4675`). A tester who has not toggled "Make me discoverable" has no identity record on the DHT. The self-test must honor this — never force-publish the user's identity to satisfy a green check.
2. **No public one-shot publish.** The pkarr wire helpers (`wire::build_relay_payload`, `wire::z32_for_verifying_key`) are `pub(crate)`; the only public publish path is the fire-and-forget background `PkarrPublisher::register`. So a *fresh timed write* probe would require an upstream `harmony-pkarr` change. **Decision (approved):** keep `publish` a client-only state-check; `resolve_self` carries the real round-trip and transitively proves the publish loop wrote a fresh, skew-valid record.

## 4. Architecture

A single production struct, built **at IPC-call time** from the locked `NodeState` (not at boot — so it reflects current state across restarts), implementing both probe traits:

```rust
// network_health.rs, beside the existing Prod*Snapshot impls.
pub struct ProdSelfTest {
    iroh_endpoint: Option<std::sync::Arc<crate::iroh_endpoint::IrohEndpoint>>,
    pkarr_resolver: Option<std::sync::Arc<harmony_pkarr::PkarrResolver>>,
    identity_pub_64: Option<[u8; 64]>,
    discoverable: bool,
    identity_publishing: bool, // active_handles() contains "identity"
}

impl IrohSelfTest for ProdSelfTest { /* endpoint_bound, relay_round_trip */ }
impl PkarrSelfTest for ProdSelfTest { /* publish_identity, resolve_self */ }
```

One struct satisfies both `&dyn IrohSelfTest` and `&dyn PkarrSelfTest`, so the IPC passes `&probes` twice. **No boot-wiring change** (`lib.rs:5490-5560` untouched) — the snapshot wiring is independent of self-test.

### IPC flow (`network_health_run_self_test`, `lib.rs:37102`)

1. Lock `NodeState`; clone the `network_health` service, `iroh_endpoint`, `pkarr_resolver`, `dm_identity_pub_64`, and the `pkarr_identity_publisher` handle + settings path. Drop the lock, then `await` `active_handles()` and read the persisted `identity_discoverable` outside the lock.
2. If the `network_health` service is `None` (node not started), return a `SelfTestReport` with all four steps `Skipped { "node not started" }` and empty `peer_results` — honest, and there is no service to run probes against. (This replaces today's behavior of returning a synthetic all-`Skipped` report regardless.)
3. Build `ProdSelfTest`; call `svc.run_self_test(&probes, &probes, &NullDispatcher).await`.
4. Return the report (already cached for export by `run_self_test`).

The synthetic-report block and its `cache_synthetic_self_test` call are deleted. `__now_ms_for_ipc` becomes unused and is removed if no other caller remains.

## 5. The four probes — exact semantics

Reason strings are bounded (≤ ~80 chars, no identifiers) per §6.2. All probes are infallible at the IPC level (they return `StepOutcome`, never `Err`); the IPC only returns `Err` on a poisoned lock.

| Step | Logic | Pass | Fail | Skipped |
|---|---|---|---|---|
| **endpoint** | `iroh_endpoint.and_then(node_id).is_some()` | `Pass { 0 }` | `Fail { "endpoint not bound" }` | — (gated only by nothing) |
| **relay** | real round-trip: `resolver.resolve(&PROBE_VK)` where `PROBE_VK` is a fixed throwaway ed25519 key → `Ok(_)` (even `None`) proves relay reachable; measure RTT | `Pass { rtt_ms }` | `Fail { "pkarr relay unreachable" }` (on resolver `Err`) | `Skipped { "endpoint not bound" }` (cascade) |
| **pkarr_publish** | state-check (three-way) | `Pass { 0 }` when `discoverable && identity_publishing` | `Fail { "identity publication not active" }` when `discoverable && !identity_publishing` (toggle on but not registered — a genuine anomaly) | `Skipped { "enable 'Make me discoverable' to test discovery" }` when `!discoverable`; `Skipped { "relay unreachable" }` (cascade) |
| **pkarr_resolve** | real round-trip: epoch-window key derivation → `resolve_window` → `verify_inner_sig`+`verify_identity_match(&id_pub)`+`verify_skew(now)`; measure RTT | `Pass { rtt_ms }` | `Fail { "identity not resolvable from pkarr" }` (None) / `Fail { "resolved record failed verification" }` | `Skipped { "publish not active" }` (cascade) |

`relay → publish → resolve` cascade is preserved: a non-`Pass` upstream forces the downstream `Skipped`, so one root cause yields one red mark, not four.

### Probe key for the relay step

A fixed, deterministic throwaway verifying key: `ed25519_dalek::SigningKey::from_bytes(&PROBE_SEED).verifying_key()` with a hardcoded `PROBE_SEED: [u8; 32]`. `resolver.resolve(&probe_vk)` round-trips to the pkarr relay pool and returns `Ok(None)` when reachable (the key is almost certainly absent), `Err(_)` only on a transport/relay failure. This is a genuine timed round-trip using the public resolver API — no `pub(crate)` wire helpers needed.

### Why "relay" probes the pkarr relay, not the iroh home relay

The self-test's purpose is the pkarr publish→resolve reachability loop, and the §6.2 cascade is only meaningful if `relay` is the precondition the pkarr steps actually depend on. iroh 0.98 exposes no relay-RTT API, and the iroh home-relay assignment is already surfaced separately on the snapshot panel (`home_relay_url`). So `relay_round_trip` (declared on `IrohSelfTest`) is implemented against the pkarr relay via the resolver handle that `ProdSelfTest` holds. This naming asymmetry is documented at the impl site.

## 6. Orchestrator tri-state refactor

`run_self_test` currently maps `Result<Duration, String>` from each probe to `Pass`/`Fail`. To let a probe self-skip (so `pkarr_publish` can show a neutral `⊘` for the opt-out case instead of a false-alarm red `✗`), the three async probe methods change their return type:

```rust
// before: BoxFuture<Result<Duration, String>>
// after:  BoxFuture<StepOutcome>
fn relay_round_trip(&self) -> BoxFuture<'_, StepOutcome>;
fn publish_identity(&self) -> BoxFuture<'_, StepOutcome>;
fn resolve_self(&self)     -> BoxFuture<'_, StepOutcome>;
```

`endpoint_bound` stays `-> bool` (a binary precondition). The orchestrator becomes: run `endpoint`; if not bound, force `relay`/`publish`/`resolve` to `Skipped`; otherwise call each probe, push its returned `StepOutcome`, and gate the next step on whether the previous outcome was `Pass`. A probe returning `Skipped` (e.g. publish when not discoverable) gates its downstream exactly like a non-pass.

This change is contained to `network_health.rs`: the orchestrator body, the `ScriptedIrohTest`/`ScriptedPkarrTest` test fakes, and the ~4 affected unit tests. The public wire types (`StepOutcome`, `SelfTestReport`, the TS mirror) are unchanged, so the frontend `NetworkHealthView` needs no change.

### Privacy guard

The self-test never calls `enable()`/`register()` or otherwise publishes the identity to make a step pass. Discoverability is read-only input. If it is off, `pkarr_publish` and (by cascade) `pkarr_resolve` are `Skipped` with an actionable reason. This preserves the user's self-sovereign opt-out.

## 7. Peer pings — deferred

Per-peer pings remain `Skipped`. The orchestrator keeps passing `NullDispatcher`; the per-peer skip reason is tidied to an honest `"per-peer ping not yet enabled"`. The existing `ping_peer` helper (`network_health.rs:927`) makes the follow-up cheap. A follow-up Linear ticket (wire `ProdPingDispatcher`) will be filed when the PR opens. Rationale: the playbook's Step 1 is single-machine (no peers), peer pings add iroh-connect latency/flakiness (5s × peers), and the four local steps are the tester trust signal ZEB-385 targets.

## 8. Testing

All in `network_health.rs` unit tests using a **real `RelayClient` against `harmony_pkarr::testing::MockPkarrRelay`** (the pattern already used by `pkarr_identity_publisher` tests), plus orchestrator tests with the updated `Scripted*` fakes:

1. **Happy path** (discoverable, published, relay up): endpoint `Pass`, relay `Pass`, publish `Pass`, resolve `Pass` — the 4×✓ result.
2. **Discoverability off**: endpoint `Pass`, relay `Pass`, publish `Skipped("enable 'Make me discoverable'…")`, resolve `Skipped("publish not active")`. Asserts no DHT write happened (publisher `active_handles()` stays empty / record absent).
3. **Relay down** (resolver `Err`): relay `Fail`, publish + resolve `Skipped` (single root cause).
4. **No endpoint**: all of relay/publish/resolve `Skipped`.
5. **Resolve verification failure**: publishable record present but identity mismatch / stale skew → resolve `Fail("resolved record failed verification")`.
6. **Discoverable-but-not-registered anomaly**: `discoverable && !identity_publishing` → publish `Fail("identity publication not active")`.
7. **Orchestrator tri-state**: a probe returning `Skipped` gates its downstream correctly (updated `Scripted*` tests).

Gates per CLAUDE.md: `cargo fmt --all -- --check`, `cargo clippy --locked -p harmony-app --lib --bins --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked -p harmony-app --lib --features test-fixtures`; final `--all-targets` sweep before PR.

## 9. Documentation

- `docs/cross-wan-validation.md` Step 1: insert "**Enable Settings → Make me discoverable**" before "Run self-test"; clarify that `pkarr_publish`/`pkarr_resolve` reflect the discoverability choice (a neutral `⊘` there means "not discoverable", not "broken"), and that `relay`/`resolve` show real RTTs so slow ≠ broken.
- `docs/release-process.md`: if the alpha smoke checklist references the self-test, align it with the real four-step behavior.

## 10. Out of scope

- Real per-peer pings (follow-up ticket; `ping_peer` already exists).
- A fresh timed *write* publish probe (needs an upstream `harmony-pkarr` public one-shot publish; `resolve_self` already proves the loop).
- iroh home-relay HTTP RTT (no iroh 0.98 API; the pkarr-relay round-trip is the meaningful precondition).
- Any "discoverable by default for alpha" policy change (separate product decision).
- Secret redaction in logs / export (already handled by the existing redaction-aware export path).

## 11. Files touched

- `src-tauri/src/network_health.rs` — `ProdSelfTest` struct + both trait impls; `StepOutcome` return-type refactor of `run_self_test` + `Scripted*` fakes + tests; tidy peer-ping skip reason.
- `src-tauri/src/lib.rs` — rewrite `network_health_run_self_test` to build `ProdSelfTest` from `NodeState` and call `run_self_test`; delete the synthetic block; drop `__now_ms_for_ipc` if now unused.
- `docs/cross-wan-validation.md`, `docs/release-process.md` — doc alignment.
- No frontend change (wire types unchanged).
- Follow-up Linear ticket: wire real per-peer pings.
