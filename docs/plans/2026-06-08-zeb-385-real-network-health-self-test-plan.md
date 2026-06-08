# ZEB-385 — Real Network Health Self-Test Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the synthetic all-`Skipped` Network Health self-test with four real probes (endpoint / pkarr-relay round-trip / publish state-check / resolve round-trip) that honor the discoverability opt-in, so a healthy node shows `✓` and a broken one shows an actionable reason.

**Architecture:** A new `ProdSelfTest` struct (in `network_health.rs`) implements the existing `IrohSelfTest` + `PkarrSelfTest` trait seam; the IPC `network_health_run_self_test` builds it from the locked `NodeState` and calls the already-tested orchestrator `NetworkHealthService::run_self_test`. The probe trait methods change their return type to the existing `StepOutcome` tri-state so a probe can self-`Skipped` (e.g. publish when not discoverable) instead of a misleading red `Fail`. `resolve_self` carries the real DHT round-trip and transitively proves publish; both pkarr probes build a *fresh* `PkarrResolver` from the relay client each call so there is no shared-cache interference.

**Tech Stack:** Rust, Tauri v2, `harmony_pkarr` (`PkarrResolver`, `RelayClient`, `PkarrRoutingRecord`, `derive_ephemeral_key`, `epoch_tolerance_window`), `ed25519_dalek`, `tokio`, `cargo-nextest`.

**Spec:** `docs/specs/2026-06-08-zeb-385-real-network-health-self-test-design.md`

**Branch:** `zeb-385-real-network-health-self-test` (already created off `origin/main` `ec70764f`).

---

## File structure

- `src-tauri/src/network_health.rs` — **modify.** Tri-state refactor of the `IrohSelfTest`/`PkarrSelfTest` traits + `run_self_test` orchestrator + `Scripted*` test fakes + existing self-test unit tests; add `ProdSelfTest` struct + impls + its unit tests.
- `src-tauri/src/lib.rs` — **modify.** Rewrite the `network_health_run_self_test` IPC (`~37102`) to build `ProdSelfTest` from `NodeState` and call `run_self_test`; add the node-not-started fallback; delete the synthetic block; remove `cache_synthetic_self_test` if it becomes unused.
- `docs/cross-wan-validation.md` — **modify.** Step 1: add "enable Make me discoverable" + clarify the four-step semantics.
- `docs/release-process.md` — **modify (if it references the self-test).**
- No frontend change — the wire types (`StepOutcome`, `SelfTestReport`) are unchanged.

## Gates (per CLAUDE.md; run from `src-tauri/`)

- Format: `cargo fmt --all -- --check`
- Lint (scoped, per-task): `cargo clippy --locked -p harmony-app --lib --bins --features test-fixtures --no-deps -- -D warnings`
- Test (scoped, per-task): `cargo nextest run --locked -p harmony-app --lib --features test-fixtures`
- Final sweep (Task 4): `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` and `cargo nextest run --locked --all-targets --features test-fixtures`

> Implementer note: **commit before running cargo gates.** Each cargo invocation can exceed 10 min on a cold build — if a single command runs longer than ~10 min wall-clock, kill it, report `DONE_WITH_CONCERNS` with what you observed, and let the controller decide. macOS contributors must have completed the `spctl developer-mode enable-terminal` setup (CLAUDE.md) or cold test runs hang.

---

## Task 1: Tri-state probe seam + orchestrator refactor

Change the three async probe methods to return `StepOutcome` (so a probe can self-`Skipped`), update the orchestrator gating, update the `Scripted*` fakes and the five existing self-test tests, and add a new test proving a probe-returned `Skipped` cascades downstream.

**Files:**
- Modify: `src-tauri/src/network_health.rs:980-995` (trait defs), `:1023-1127` (orchestrator steps), `:2032-2065` (`Scripted*` fakes), `:2078-2198` (existing self-test tests).

- [ ] **Step 1: Change the three probe trait method signatures**

Replace the bodies of the two traits (currently `network_health.rs:974-995`) so the async methods return `StepOutcome` instead of `Result<Duration, String>`:

```rust
pub trait IrohSelfTest: Send + Sync {
    /// True if the iroh endpoint is bound (Phase 1: any endpoint present).
    fn endpoint_bound(&self) -> bool;
    /// Round-trip reachability probe to the pkarr relay. Returns a
    /// `StepOutcome` directly so the probe owns its duration / reason.
    fn relay_round_trip(&self) -> futures::future::BoxFuture<'_, StepOutcome>;
}

pub trait PkarrSelfTest: Send + Sync {
    /// State-check: is the identity publication active? Returns `Skipped`
    /// when the user has not enabled discoverability (not a failure).
    fn publish_identity(&self) -> futures::future::BoxFuture<'_, StepOutcome>;
    /// Resolve own identity from pkarr and verify the returned record.
    fn resolve_self(&self) -> futures::future::BoxFuture<'_, StepOutcome>;
}
```

- [ ] **Step 2: Rewrite the four-step body of `run_self_test`**

Replace the four step-building blocks (currently `network_health.rs:1033-1127`, from `// Step 1: endpoint` through the end of the Step-4 block, i.e. just before `// Per-peer pings:`) with this tri-state gating. **Leave the per-peer ping block (everything from `// Per-peer pings:` onward) unchanged.**

```rust
        // Step 1: endpoint (binary precondition).
        let endpoint_ok = iroh_test.endpoint_bound();
        steps.push(SelfTestStep {
            name: "endpoint".into(),
            outcome: if endpoint_ok {
                StepOutcome::Pass { duration_ms: 0 }
            } else {
                StepOutcome::Fail {
                    reason: "endpoint not bound".into(),
                }
            },
        });

        // Step 2: relay (gated on endpoint). The probe owns its outcome.
        let relay_outcome = if endpoint_ok {
            iroh_test.relay_round_trip().await
        } else {
            StepOutcome::Skipped {
                reason: "skipped: endpoint not bound".into(),
            }
        };
        let relay_ok = matches!(relay_outcome, StepOutcome::Pass { .. });
        steps.push(SelfTestStep {
            name: "relay".into(),
            outcome: relay_outcome,
        });

        // Step 3: pkarr_publish (gated on relay). The probe may itself
        // return Skipped (e.g. discoverability off) — that gates resolve.
        let publish_outcome = if relay_ok {
            pkarr_test.publish_identity().await
        } else {
            StepOutcome::Skipped {
                reason: "skipped: relay unreachable".into(),
            }
        };
        let publish_ok = matches!(publish_outcome, StepOutcome::Pass { .. });
        steps.push(SelfTestStep {
            name: "pkarr_publish".into(),
            outcome: publish_outcome,
        });

        // Step 4: pkarr_resolve (gated on publish) — the real round-trip.
        let resolve_outcome = if publish_ok {
            pkarr_test.resolve_self().await
        } else {
            StepOutcome::Skipped {
                reason: "skipped: publish not completed".into(),
            }
        };
        steps.push(SelfTestStep {
            name: "pkarr_resolve".into(),
            outcome: resolve_outcome,
        });
```

- [ ] **Step 3: Update the `Scripted*` test fakes**

Replace `network_health.rs:2032-2065` so the fakes carry a `StepOutcome` instead of a `Result<Duration, String>`:

```rust
    struct ScriptedIrohTest {
        bound: bool,
        relay: StepOutcome,
    }
    impl IrohSelfTest for ScriptedIrohTest {
        fn endpoint_bound(&self) -> bool {
            self.bound
        }
        fn relay_round_trip(&self) -> futures::future::BoxFuture<'_, StepOutcome> {
            let r = self.relay.clone();
            async move { r }.boxed()
        }
    }

    struct ScriptedPkarrTest {
        publish: StepOutcome,
        resolve: StepOutcome,
    }
    impl PkarrSelfTest for ScriptedPkarrTest {
        fn publish_identity(&self) -> futures::future::BoxFuture<'_, StepOutcome> {
            let r = self.publish.clone();
            async move { r }.boxed()
        }
        fn resolve_self(&self) -> futures::future::BoxFuture<'_, StepOutcome> {
            let r = self.resolve.clone();
            async move { r }.boxed()
        }
    }
```

- [ ] **Step 4: Update the five existing self-test tests' fake construction**

In each existing test (`self_test_all_pass_path`, `self_test_relay_fail_cascades_downstream_to_skipped`, `self_test_endpoint_unbound_all_steps_skipped`, `self_test_pkarr_resolve_mismatch_reported_as_fail`, `self_test_result_is_cached_for_export`), change the `ScriptedIrohTest`/`ScriptedPkarrTest` literals from `Ok(...)`/`Err(...)` to `StepOutcome` variants. The mapping:
- `relay: Ok(Duration::from_millis(24))` → `relay: StepOutcome::Pass { duration_ms: 24 }`
- `relay: Err("relay timeout after 5s".into())` → `relay: StepOutcome::Fail { reason: "relay timeout after 5s".into() }`
- `relay: Ok(Duration::from_millis(0))` → `relay: StepOutcome::Pass { duration_ms: 0 }`
- `publish: Ok(Duration::from_millis(380))` → `publish: StepOutcome::Pass { duration_ms: 380 }`
- `resolve: Ok(Duration::from_millis(210))` → `resolve: StepOutcome::Pass { duration_ms: 210 }`
- `resolve: Err("pkarr resolved unexpected payload".into())` → `resolve: StepOutcome::Fail { reason: "pkarr resolved unexpected payload".into() }`
- `publish: Ok(Duration::from_millis(0))` / `resolve: Ok(Duration::from_millis(0))` → `StepOutcome::Pass { duration_ms: 0 }`

All assertions (`matches!(report.steps[i].outcome, StepOutcome::Pass/Fail/Skipped { .. })`) stay exactly as they are. The `self_test_pkarr_resolve_mismatch_reported_as_fail` assertion `assert_eq!(reason, "pkarr resolved unexpected payload")` still holds because the orchestrator now pushes the probe's own `Fail { reason }` through verbatim.

- [ ] **Step 5: Add a new test — a probe-returned `Skipped` gates downstream**

Add this test in the same `#[cfg(test)] mod tests` block, next to the other `self_test_*` tests:

```rust
    #[tokio::test]
    async fn self_test_publish_self_skip_cascades_resolve_to_skipped() {
        // Probe returns Skipped on publish (e.g. discoverability off); the
        // orchestrator must mark pkarr_resolve Skipped, NOT run it.
        let svc = build_svc_for_self_test();
        let iroh_t = ScriptedIrohTest {
            bound: true,
            relay: StepOutcome::Pass { duration_ms: 12 },
        };
        let pkarr_t = ScriptedPkarrTest {
            publish: StepOutcome::Skipped {
                reason: "enable 'Make me discoverable' to test discovery".into(),
            },
            resolve: StepOutcome::Pass { duration_ms: 99 },
        };
        let report = svc.run_self_test(&iroh_t, &pkarr_t, &NullDispatcher).await;
        assert!(matches!(report.steps[1].outcome, StepOutcome::Pass { .. }));
        assert!(
            matches!(report.steps[2].outcome, StepOutcome::Skipped { .. }),
            "publish self-skipped"
        );
        assert!(
            matches!(report.steps[3].outcome, StepOutcome::Skipped { .. }),
            "resolve skipped because publish did not pass"
        );
    }
```

- [ ] **Step 6: Format, then run the scoped gates**

```bash
cd src-tauri
git add -A && git commit -q -m "refactor(zeb-385): self-test probes return StepOutcome tri-state

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
cargo fmt --all -- --check
cargo clippy --locked -p harmony-app --lib --bins --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(self_test)'
```

Expected: fmt clean; clippy 0 warnings; the six `self_test_*` tests pass. If fmt rewrites files, `git add -A && git commit --amend --no-edit` and re-check.

---

## Task 2: `ProdSelfTest` production probes + IPC wiring

Add the production probe struct and wire the IPC to use it. Born wired (struct + IPC use land together) so there is no unused-`pub`-item window.

**Files:**
- Modify: `src-tauri/src/network_health.rs` (add `ProdSelfTest` after the `NullDispatcher` impl, `~1232`; add probe tests in the test module).
- Modify: `src-tauri/src/lib.rs` (rewrite `network_health_run_self_test`, `~37088-37152`).

- [ ] **Step 1: Write failing probe unit tests (against a real relay)**

Add these tests in `network_health.rs`'s `#[cfg(test)] mod tests` block. They reference `ProdSelfTest`, which does not exist yet, so they fail to compile (expected). Put `use std::sync::Arc;` at the top of the test fn bodies if `Arc` is not already in scope in the module.

```rust
    // ── ZEB-385: ProdSelfTest probe tests (real RelayClient + mock relay) ──

    #[tokio::test]
    async fn prod_relay_round_trip_reachable_relay_passes() {
        use harmony_pkarr::{testing::MockPkarrRelay, RelayClient, RelayPool};
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = std::sync::Arc::new(RelayClient::new(pool));
        let probes = ProdSelfTest {
            iroh_endpoint: None,
            pkarr_relay_client: Some(client),
            identity_pub_64: None,
            discoverable: false,
            identity_publishing: false,
        };
        assert!(
            matches!(probes.relay_round_trip().await, StepOutcome::Pass { .. }),
            "reachable mock relay -> Pass"
        );
    }

    #[tokio::test]
    async fn prod_relay_round_trip_dead_relay_fails() {
        use harmony_pkarr::{RelayClient, RelayPool};
        // Port 1 is unbindable / unreachable; the GET round-trip errors.
        let pool = RelayPool::new(vec!["http://127.0.0.1:1".to_string()]);
        let client = std::sync::Arc::new(RelayClient::new(pool));
        let probes = ProdSelfTest {
            iroh_endpoint: None,
            pkarr_relay_client: Some(client),
            identity_pub_64: None,
            discoverable: false,
            identity_publishing: false,
        };
        assert!(
            matches!(probes.relay_round_trip().await, StepOutcome::Fail { .. }),
            "dead relay -> Fail"
        );
    }

    #[tokio::test]
    async fn prod_publish_identity_state_check_three_ways() {
        let mk = |discoverable, identity_publishing| ProdSelfTest {
            iroh_endpoint: None,
            pkarr_relay_client: None,
            identity_pub_64: None,
            discoverable,
            identity_publishing,
        };
        assert!(
            matches!(mk(false, false).publish_identity().await, StepOutcome::Skipped { .. }),
            "not discoverable -> Skipped"
        );
        assert!(
            matches!(mk(true, true).publish_identity().await, StepOutcome::Pass { .. }),
            "discoverable + registered -> Pass"
        );
        assert!(
            matches!(mk(true, false).publish_identity().await, StepOutcome::Fail { .. }),
            "discoverable but not registered -> Fail"
        );
    }

    #[tokio::test]
    async fn prod_resolve_self_absent_identity_fails() {
        use harmony_pkarr::{testing::MockPkarrRelay, RelayClient, RelayPool};
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = std::sync::Arc::new(RelayClient::new(pool));
        let id_sk = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let mut id_pub = [0u8; 64];
        id_pub[32..].copy_from_slice(&id_sk.verifying_key().to_bytes());
        let probes = ProdSelfTest {
            iroh_endpoint: None,
            pkarr_relay_client: Some(client),
            identity_pub_64: Some(id_pub),
            discoverable: true,
            identity_publishing: true,
        };
        // Nothing published for this identity -> not resolvable -> Fail.
        assert!(matches!(probes.resolve_self().await, StepOutcome::Fail { .. }));
    }

    #[tokio::test]
    async fn prod_resolve_self_finds_published_identity() {
        use harmony_pkarr::{
            current_epoch_id, derive_ephemeral_key, testing::MockPkarrRelay, EphemeralKeyBuilder,
            PkarrCase, PkarrPublisher, PkarrRoutingRecord, RecordBuilder, RelayClient, RelayPool,
        };
        let relay = MockPkarrRelay::start().await;
        let pool = RelayPool::new(vec![relay.base_url.clone()]);
        let client = std::sync::Arc::new(RelayClient::new(pool));
        let publisher = std::sync::Arc::new(PkarrPublisher::new(std::sync::Arc::clone(&client)));
        let _ph = std::sync::Arc::clone(&publisher).spawn();

        let id_sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let mut id_pub = [0u8; 64];
        id_pub[32..].copy_from_slice(&id_sk.verifying_key().to_bytes());

        // Register the identity publication (mirrors PkarrIdentityPublisher::enable).
        let id_pub_for_key = id_pub;
        let key_builder: EphemeralKeyBuilder = std::sync::Arc::new(move |at_ms| {
            let epoch_id = current_epoch_id(at_ms);
            derive_ephemeral_key(PkarrCase::Identity, &id_pub_for_key, &epoch_id.to_be_bytes())
        });
        let id_sk2 = id_sk.clone();
        let builder: RecordBuilder = std::sync::Arc::new(move |at_ms| {
            PkarrRoutingRecord::sign_new(b"routing".to_vec(), id_pub, at_ms, &id_sk2)
                .expect("sign")
        });
        publisher
            .register("identity".to_string(), key_builder, builder)
            .await;

        let probes = ProdSelfTest {
            iroh_endpoint: None,
            pkarr_relay_client: Some(std::sync::Arc::clone(&client)),
            identity_pub_64: Some(id_pub),
            discoverable: true,
            identity_publishing: true,
        };
        // resolve_self builds a FRESH resolver each call (no stale cache), so
        // polling works: wait for the background publish to land on the relay.
        let mut found = false;
        for _ in 0..40 {
            if matches!(probes.resolve_self().await, StepOutcome::Pass { .. }) {
                found = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(found, "published identity became resolvable -> Pass");
    }

    #[tokio::test]
    async fn prod_endpoint_bound_false_when_no_endpoint() {
        let probes = ProdSelfTest {
            iroh_endpoint: None,
            pkarr_relay_client: None,
            identity_pub_64: None,
            discoverable: false,
            identity_publishing: false,
        };
        assert!(!probes.endpoint_bound());
    }
```

- [ ] **Step 2: Run the probe tests to confirm they fail to compile**

```bash
cd src-tauri
cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(prod_)' 2>&1 | tail -20
```

Expected: compile error — `cannot find struct ProdSelfTest`.

- [ ] **Step 3: Implement `ProdSelfTest`**

Add this immediately after the `NullDispatcher` impl block in `network_health.rs` (after `~1232`, before the `// ── Production trait impls` comment for the snapshot adapters):

```rust
// ── ZEB-385: production self-test probes ────────────────────────────
//
// Built at IPC-call time from the locked `NodeState`; holds cheap
// `Arc`/copy handles. Both pkarr probes build a FRESH `PkarrResolver`
// from the relay client each call so the self-test reflects current
// reachability (no shared-cache hits / stale positives or negatives).
//
// `relay_round_trip` is declared on `IrohSelfTest` but probes the
// **pkarr** relay (the precondition the pkarr publish/resolve steps
// depend on): iroh 0.98 exposes no relay-RTT API, and iroh home-relay
// assignment is surfaced separately on the snapshot panel.
pub struct ProdSelfTest {
    pub iroh_endpoint: Option<std::sync::Arc<crate::iroh_endpoint::IrohEndpoint>>,
    pub pkarr_relay_client: Option<std::sync::Arc<harmony_pkarr::RelayClient>>,
    pub identity_pub_64: Option<[u8; 64]>,
    pub discoverable: bool,
    pub identity_publishing: bool,
}

impl IrohSelfTest for ProdSelfTest {
    fn endpoint_bound(&self) -> bool {
        // A present endpoint is a bound endpoint (node_id() is infallible).
        self.iroh_endpoint.is_some()
    }

    fn relay_round_trip(&self) -> futures::future::BoxFuture<'_, StepOutcome> {
        Box::pin(async move {
            let Some(relay) = self.pkarr_relay_client.as_ref() else {
                return StepOutcome::Fail {
                    reason: "pkarr relay client not initialized".into(),
                };
            };
            // Fresh resolver -> empty cache -> a real round-trip every run. A
            // random throwaway key is almost certainly absent, so a reachable
            // relay returns Ok(None); only a transport failure returns Err.
            let resolver = harmony_pkarr::PkarrResolver::new(std::sync::Arc::clone(relay));
            let probe_vk =
                ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng).verifying_key();
            let start = std::time::Instant::now();
            match resolver.resolve(&probe_vk).await {
                Ok(_) => StepOutcome::Pass {
                    duration_ms: start.elapsed().as_millis() as u32,
                },
                Err(_) => StepOutcome::Fail {
                    reason: "pkarr relay unreachable".into(),
                },
            }
        })
    }
}

impl PkarrSelfTest for ProdSelfTest {
    fn publish_identity(&self) -> futures::future::BoxFuture<'_, StepOutcome> {
        let discoverable = self.discoverable;
        let publishing = self.identity_publishing;
        Box::pin(async move {
            if !discoverable {
                StepOutcome::Skipped {
                    reason: "enable 'Make me discoverable' to test discovery".into(),
                }
            } else if publishing {
                StepOutcome::Pass { duration_ms: 0 }
            } else {
                StepOutcome::Fail {
                    reason: "identity publication not active".into(),
                }
            }
        })
    }

    fn resolve_self(&self) -> futures::future::BoxFuture<'_, StepOutcome> {
        Box::pin(async move {
            let Some(relay) = self.pkarr_relay_client.as_ref() else {
                return StepOutcome::Fail {
                    reason: "pkarr relay client not initialized".into(),
                };
            };
            let Some(id_pub) = self.identity_pub_64 else {
                return StepOutcome::Fail {
                    reason: "identity not loaded".into(),
                };
            };
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let verifying_keys: Vec<_> = harmony_pkarr::epoch_tolerance_window(now_ms)
                .iter()
                .map(|&epoch| {
                    harmony_pkarr::derive_ephemeral_key(
                        harmony_pkarr::PkarrCase::Identity,
                        &id_pub,
                        &epoch.to_be_bytes(),
                    )
                    .verifying_key()
                })
                .collect();
            let resolver = harmony_pkarr::PkarrResolver::new(std::sync::Arc::clone(relay));
            let start = std::time::Instant::now();
            match resolver.resolve_window(&verifying_keys).await {
                Ok(Some(rec)) => {
                    if rec.verify_inner_sig().is_err()
                        || rec.verify_identity_match(&id_pub).is_err()
                        || rec.verify_skew(now_ms).is_err()
                    {
                        StepOutcome::Fail {
                            reason: "resolved record failed verification".into(),
                        }
                    } else {
                        StepOutcome::Pass {
                            duration_ms: start.elapsed().as_millis() as u32,
                        }
                    }
                }
                Ok(None) => StepOutcome::Fail {
                    reason: "identity not resolvable from pkarr".into(),
                },
                Err(_) => StepOutcome::Fail {
                    reason: "pkarr resolve failed".into(),
                },
            }
        })
    }
}
```

- [ ] **Step 4: Run the probe tests to confirm they pass**

```bash
cd src-tauri
cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(prod_)'
```

Expected: all seven `prod_*` tests pass. `prod_resolve_self_finds_published_identity` may take up to ~2s (polling for the publish to land).

- [ ] **Step 5: Rewrite the `network_health_run_self_test` IPC**

Replace the entire IPC body (`lib.rs:37088-37152`, from the doc comment `/// Spec §5.3 + §6.1. ...` through the closing `}` of the function — the whole synthetic block) with:

```rust
/// Spec §5.3 + §6.1. Returns Err only on truly exceptional cases
/// (NodeState lock poisoned). Step failures live inside the report.
///
/// ZEB-385: builds `ProdSelfTest` from the locked `NodeState` and runs
/// the real four-step probe sequence (endpoint / pkarr-relay round-trip /
/// publish state-check / resolve round-trip). Honors the "Make me
/// discoverable" opt-in — publish/resolve `Skipped` (not failed) when off.
#[tauri::command(rename_all = "snake_case")]
async fn network_health_run_self_test(
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<crate::network_health::SelfTestReport, String> {
    // Snapshot every handle we need under the lock, then drop it before awaiting.
    let (svc, iroh_endpoint, pkarr_relay_client, identity_pub_64, publisher, settings_path) = {
        let g = state
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.network_health.clone(),
            g.iroh_endpoint.clone(),
            g.pkarr_relay_client.clone(),
            g.dm_identity_pub_64,
            g.pkarr_publisher.clone(),
            g.pkarr_settings_path.clone(),
        )
    };

    // Node not started: no service to probe — return an honest all-Skipped report.
    let Some(svc) = svc else {
        let now = crate::network_health::__now_ms_for_ipc();
        let steps = ["endpoint", "relay", "pkarr_publish", "pkarr_resolve"]
            .iter()
            .map(|name| crate::network_health::SelfTestStep {
                name: (*name).to_string(),
                outcome: crate::network_health::StepOutcome::Skipped {
                    reason: "node not started".into(),
                },
            })
            .collect();
        return Ok(crate::network_health::SelfTestReport {
            started_at_ms: now,
            finished_at_ms: now,
            steps,
            peer_results: vec![],
        });
    };

    // Discoverability (persisted) + whether the identity publication is registered.
    let discoverable = match settings_path {
        Some(p) => pkarr_settings::PkarrSettings::load_or_default(&p).identity_discoverable,
        None => false,
    };
    let identity_publishing = match publisher.as_ref() {
        // "identity" is the HANDLE const in pkarr_identity_publisher.rs.
        Some(p) => p.active_handles().await.iter().any(|h| h == "identity"),
        None => false,
    };

    let probes = crate::network_health::ProdSelfTest {
        iroh_endpoint,
        pkarr_relay_client,
        identity_pub_64,
        discoverable,
        identity_publishing,
    };

    // run_self_test caches the report internally for network_health_export_payload.
    Ok(svc
        .run_self_test(&probes, &probes, &crate::network_health::NullDispatcher)
        .await)
}
```

- [ ] **Step 6: Remove `cache_synthetic_self_test` if now unused**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
grep -rn "cache_synthetic_self_test" src-tauri/src/
```

If the only remaining hit is the definition in `network_health.rs` (no callers), delete the `pub async fn cache_synthetic_self_test(...)` method and its doc comment. If a test or other caller references it, leave it. Then confirm `__now_ms_for_ipc` still has a caller (the node-not-started branch above) — it does, so keep it.

- [ ] **Step 7: Commit, format, and run the scoped gates**

```bash
cd src-tauri
git add -A && git commit -q -m "feat(zeb-385): real Network Health self-test probes + IPC wiring

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
cargo fmt --all -- --check
cargo clippy --locked -p harmony-app --lib --bins --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(self_test) + test(prod_)'
```

Expected: fmt clean; clippy 0 warnings; all `self_test_*` + `prod_*` tests pass. If fmt rewrites, amend the commit and re-check.

---

## Task 3: Documentation + follow-up ticket

**Files:**
- Modify: `docs/cross-wan-validation.md` (Step 1).
- Modify: `docs/release-process.md` (if it references the self-test — check first).

- [ ] **Step 1: Fix `docs/cross-wan-validation.md` Step 1**

Replace the current Step 1 list (`docs/cross-wan-validation.md:19-26`) with:

```markdown
On EACH machine independently:

1. Launch Harmony.
2. Open the **Network** panel (sidebar → Network).
3. Enable **Settings → Make me discoverable** (publishes your identity to
   pkarr so the pkarr self-test steps can verify discovery). Leave it on for
   the whole test.
4. Wait until the "Reachable" status appears (typically <30 seconds).
5. Click **Run self-test**. With discoverability on and a healthy network,
   all four steps show ✓:
   - **endpoint** — your iroh endpoint is bound.
   - **relay** — round-trip to the pkarr relay (shows the real RTT in ms; a
     slow but non-zero RTT is fine, not a failure).
   - **pkarr_publish** — your identity publication is active.
   - **pkarr_resolve** — your identity resolved back from pkarr (real RTT).

   A neutral **⊘** on `pkarr_publish`/`pkarr_resolve` means discoverability is
   off (turn it on in step 3) — it is **not** a failure. A red **✗** is a real
   problem; the reason next to it says what.
6. Screenshot the panel for your records.
```

- [ ] **Step 2: Check + align `docs/release-process.md`**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
grep -n "self-test\|self test\|Network Health\|expect.*✓" docs/release-process.md
```

If the alpha smoke checklist references the self-test as "all steps ✓", add a parenthetical that `pkarr_publish`/`pkarr_resolve` require "Make me discoverable" enabled. If there is no such reference, make no change.

- [ ] **Step 3: Commit the docs**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add docs/cross-wan-validation.md docs/release-process.md
git commit -q -m "docs(zeb-385): self-test playbook reflects real four-step probes

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

- [ ] **Step 4: File the peer-ping follow-up ticket (controller action, at PR time)**

The controller (not the implementer subagent) files a Linear ticket in the Zeblith team / Harmony Client v1 project: *"harmony-client: wire real per-peer pings into Network Health self-test"* — body: self-test currently leaves per-peer pings `Skipped`; wire a `ProdPingDispatcher` around the existing `ping_peer` (`network_health.rs:927`) so the self-test pings known peers (5s timeout, 32-concurrent). Surfaced by ZEB-385. Use the assigned ID in the PR description; do not invent an ID.

---

## Task 4: Final sweep, push, PR

**Files:** none (verification + git).

- [ ] **Step 1: Full `--all-targets` gate sweep**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --all-targets --features test-fixtures -E 'test(self_test) + test(prod_)'
```

Expected: fmt clean; clippy 0 warnings across all targets; the self-test/prod tests pass. (Known-nonblocking iroh/zenoh first-bind flakes in unrelated transport tests may appear on a broader run; re-run once if so.)

- [ ] **Step 2: Frontend untouched — quick confirm**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git diff --name-only origin/main | grep -E '\.(ts|svelte)$' || echo "no frontend files changed (expected)"
```

Expected: "no frontend files changed". (Wire types unchanged; no `tsc`/`vitest` needed, but they are part of CI regardless.)

- [ ] **Step 3: Push and open the PR**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git push -u origin zeb-385-real-network-health-self-test
gh pr create --repo zeblithic/harmony-client --title "ZEB-385: real Network Health self-test (replace synthetic all-skipped)" --body "$(cat <<'EOF'
## Summary
Replaces the synthetic all-`⊘ skipped` Network Health self-test with four real probes, so a healthy node shows `✓` (matching the cross-WAN playbook) and a broken one shows an actionable reason. Backend-only Rust + one doc; no frontend change (wire types unchanged).

- **endpoint** — iroh endpoint bound.
- **relay** — real round-trip to the pkarr relay (fresh resolver → real RTT).
- **pkarr_publish** — state-check: identity publication active (honors the "Make me discoverable" opt-in; neutral `⊘` when off, not a red `✗`).
- **pkarr_resolve** — the real DHT round-trip: resolve own identity + verify sig/identity/skew. Transitively proves publish.

Client-only (no upstream `harmony-pkarr` change); never force-publishes the user's identity to satisfy a green check. Small `StepOutcome` tri-state refactor of the (already-tested) orchestrator lets a probe self-`Skipped`.

## Design / plan
- Spec: `docs/specs/2026-06-08-zeb-385-real-network-health-self-test-design.md`
- Plan: `docs/plans/2026-06-08-zeb-385-real-network-health-self-test-plan.md`

## Test plan
- [x] `cargo fmt --all -- --check`
- [x] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [x] `cargo nextest run --locked -p harmony-app --lib --features test-fixtures` (self_test_* + prod_* probe tests against a real `RelayClient` + `MockPkarrRelay`)
- Manual: enable "Make me discoverable", Run self-test → 4×✓; disable → publish/resolve show neutral `⊘`.

Closes ZEB-385.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Expected: PR created. Capture the PR number for the autonomous bot-review loop.

---

## Self-review notes (author)

- **Spec coverage:** §4 architecture → Task 2 (ProdSelfTest + IPC). §5 four probes → Task 2 Step 3 + tests. §6 tri-state refactor + privacy guard → Task 1 + Task 2 (publish self-skip). §7 peer pings deferred → orchestrator block unchanged + Task 3 Step 4 follow-up. §8 testing → Task 1 Step 5 + Task 2 Step 1. §9 docs → Task 3. §4 node-not-started fallback → Task 2 Step 5.
- **Type consistency:** probe methods return `futures::future::BoxFuture<'_, StepOutcome>` everywhere (traits, `Scripted*` fakes, `ProdSelfTest`). `ProdSelfTest` field names (`pkarr_relay_client`, `identity_pub_64`, `discoverable`, `identity_publishing`, `iroh_endpoint`) are identical across the struct def, all tests, and the IPC constructor. `run_self_test` step names (`endpoint`/`relay`/`pkarr_publish`/`pkarr_resolve`) match the node-not-started fallback and the existing assertions.
- **No placeholders:** every code step is complete and compile-ready against the confirmed `harmony_pkarr` / `ed25519_dalek` signatures.
