# ZEB-619: iroh 0.98.2 → 1.0.1 Upgrade Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Upgrade the pinned iroh dependency from 0.98.2 to 1.0.1, retiring the ZEB-617 manual stable-relay pin in favor of 1.0's stable defaults, and revalidating the transport stack (ZEB-616 reconnect semantics, e2e scenarios, live smoke). Closes ZEB-615 (MaxPathIdReached no-recovery — fixed upstream at 1.0.0-rc.1).

**Architecture:** Single-crate dependency surgery. `iroh` has exactly one reverse dep (`harmony-app`); the vendored `zenoh-link` fork is iroh-agnostic (factory-closure seam — zero migration); the `harmony-tunnel`/`harmony-pkarr` git deps don't pull iroh. All churn concentrates in `src-tauri/src/iroh_endpoint.rs` (builder hub + relay pin) plus whatever `cargo check --all-targets` surfaces (expected: minimal — every API we call is name-stable across this delta; the big 1.0 breaks are in surfaces we don't use).

**Tech Stack:** Rust (MSRV moves 1.88 → 1.91; toolchain pin is 1.94.1, already sufficient), cargo/nextest, e2e-harness (real-transport scenarios).

## Global Constraints

- Branch: `zeb-619-iroh-1-0-upgrade` (already created off main `b796d0c0`). Commit per task; conventional-commit style (`feat:`/`chore:`/`test:` + ZEB-619 in subject or body).
- Every cargo command includes `--locked` EXCEPT the explicit `cargo update` step in Task 1 (which exists to rewrite the lockfile).
- Per-task gates use `-p harmony-app --lib`; the FULL `--all-targets` sweep runs once, in Task 3 (a lib change relinks ~97 integration binaries — ~50 min if repeated per task).
- Every test/clippy invocation includes `--features test-fixtures` when `--all-targets` is present.
- Implementer contract: commit work BEFORE running long gates; 10-minute wall-clock kill switch on any single gate command (background + poll if longer); `DONE_WITH_CONCERNS` status is available and preferred over silent hangs.
- **NEVER restart the running fleet-koya serve process (harmony-app `--profile fleet-koya`, currently pid 43880) on this branch's build.** iroh v1 is wire-frozen: a 1.0 node CANNOT talk to the 0.98 fleet. The live smoke (Task 5) uses an isolated `HARMONY_PROFILE` instead. Fleet-wide rebuild happens post-merge, coordinated.
- Keychain isolation for any live-node step: `HARMONY_DISABLE_KEYCHAIN=1` + `HARMONY_PASSPHRASE=<any>` + `HARMONY_PROFILE=<isolated-name>`.
- MSRV target value: `rust-version = "1.91"` (iroh 1.0.1's floor). CI's msrv job reads this field from Cargo.toml automatically — do NOT edit `.github/workflows/ci.yml`.
- Target iroh version: `1.0.1` exactly (latest on crates.io as of 2026-07-01; 1.0.0→1.0.1 is fixes-only).
- Research references (read-only, in scratchpad): `zeb-619-iroh-usage-survey.md` (call-site inventory), `zeb-619-iroh-1-0-migration-research.md` (upstream delta). Key verdicts: `EndpointAddr`/`TransportAddr`/`RelayUrl` serde shapes are byte-identical (persisted reachability records need NO migration); no iroh error enums are matched anywhere in-tree; the path-observation API rewrite affects zero call sites.

---

### Task 1: Dependency bump + MSRV + lockfile regeneration

**Files:**
- Modify: `src-tauri/Cargo.toml` (~line 7 `rust-version`; lines ~160-167 iroh dep block)
- Modify: `src-tauri/Cargo.lock` (via cargo update, not by hand)

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces: a tree where `cargo check -p harmony-app --lib` passes against iroh 1.0.1. Task 2+ build on this.

- [ ] **Step 1: Edit the manifest**

In `src-tauri/Cargo.toml`, change `rust-version = "1.88"` to:

```toml
rust-version = "1.91"
```

(If a comment near it explains the previous floor, update it to: `# 1.91 = iroh 1.0.x MSRV (ZEB-619)`.)

Replace the iroh dependency block (the multi-line comment + `iroh = "0.98"`) with:

```toml
# ZEB-321 Phase 1 Task 4 (upgraded ZEB-619): iroh endpoint used by the
# zenoh-over-iroh transport and the harmony device handshakes. Pinned at
# 1.0 (wire-frozen major: any v1.x endpoint interoperates with any other
# v1.x; v1 CANNOT talk to v0.98 — fleet upgrades are flag-day). The
# harmony-* workspace crates we depend on do NOT pull iroh transitively
# (verified 2026-07-01), so we have free choice of version. MSRV floor
# 1.91 comes from this crate.
iroh = "1.0"
```

- [ ] **Step 2: Regenerate the lock for the iroh subtree**

```bash
cd src-tauri && cargo update -p iroh --precise 1.0.1
```

Expected: iroh 0.98.2→1.0.1 plus transitive moves (`iroh-base`, `iroh-relay`, `iroh-dns` → 1.0.x; `noq`/`noq-proto`/`noq-udp` → 1.0.x; `n0-watcher` → 1.0.0; `netwatch` → 0.19.x). If cargo refuses because sibling crates need simultaneous bumps, widen to the minimal set with additional `-p` flags (e.g. `cargo update -p iroh -p iroh-base -p iroh-relay`) — do NOT run a bare `cargo update` (it would churn the whole graph).

- [ ] **Step 3: Verify pkarr did not fork**

```bash
grep -c '^name = "pkarr"' Cargo.lock
```

Expected: `1`. If `2`, iroh 1.0's pkarr requirement forked against harmony-pkarr's `pkarr 3.10` — STOP and report BLOCKED with both resolved versions (unification strategy is a controller decision).

- [ ] **Step 4: Compile the lib and fix fallout**

```bash
cargo check --locked -p harmony-app --lib --features test-fixtures
```

Expected: clean or near-clean. Known-possible fallout (from the research pass): `Connection::closed()` now resolves to an `iroh::endpoint::Closed` struct — our call sites only `.await` it without binding the value, which still compiles; any `#[non_exhaustive]` enum match would need a `_ =>` arm, but the survey found no iroh enum matches in-tree. Fix anything that surfaces mechanically, staying within existing idiom. If a fix requires design judgment (not a mechanical rename), report DONE_WITH_CONCERNS describing it.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock && git commit -m "chore: upgrade iroh 0.98.2 -> 1.0.1, MSRV 1.88 -> 1.91 (ZEB-619)"
```

---

### Task 2: Retire the ZEB-617 stable-relay pin; guard 1.0's defaults with a test

Context: iroh 0.98.2's `presets::N0` hard-coded n0's CANARY relay cluster; ZEB-617 pinned us off it via `stable_relay_mode()` (`RelayMode::custom(...)` over 4 stable hostnames). iroh 1.0's `presets::N0` defaults to those same stable hosts (`use1-1.relay.n0.iroh.link.` etc. — verified in 1.0.1 `defaults.rs`), so the pin is now redundant AND the ticket explicitly supersedes it. We delete the pin but KEEP a schema-pin-style regression test asserting the defaults stay off canary (canary relays are decommissioned 2026-09-30; a future preset regression must fail loudly here, like the original ZEB-617 test did).

**Files:**
- Modify: `src-tauri/src/iroh_endpoint.rs` (const at ~127-132, `stable_relay_mode()` at ~137-142, builder chain at ~149-152, module docs at ~19-36 and ~119-135, test at ~441-461)

**Interfaces:**
- Consumes: Task 1's compiling tree.
- Produces: production builder is plain `Endpoint::builder(presets::N0).secret_key(...)...` (no `.relay_mode()` call on the production path); test `default_relay_map_is_stable_non_canary` guards the preset.

- [ ] **Step 1: Rewrite the pin test (test-first)**

Replace `stable_relay_mode_pins_four_non_canary_relays` with a test of the DEFAULT map. Sketch (implementer adapts accessor names to 1.0.1's actual API — in 0.98.2 the pattern was `RelayMode::custom(...).relay_map()` then `map.urls::<Vec<_>>()`; 1.0.1 exposes the default map via `RelayMode::Default.relay_map()` or the `iroh::defaults` module — verify against docs.rs/iroh/1.0.1 and use whichever is public):

```rust
/// ZEB-617 regression guard, retargeted by ZEB-619: iroh 1.0's default
/// (preset N0) relay map must be the stable production cluster. 0.98's
/// preset silently put the fleet on n0's CANARY relays (no SLA,
/// decommissioned 2026-09-30); if a future iroh bump regresses the
/// default, this must fail loudly.
#[test]
fn default_relay_map_is_stable_non_canary() {
    let map = iroh::RelayMode::Default.relay_map();
    let urls: Vec<String> = map.urls::<Vec<_>>().iter().map(|u| u.to_string()).collect();
    assert!(!urls.is_empty(), "default relay map must not be empty");
    for url in &urls {
        assert!(!url.contains("canary"), "canary relay leaked into defaults: {url}");
        assert!(url.contains(".relay.n0.iroh.link."), "unexpected relay host: {url}");
    }
}
```

Run it BEFORE deleting the pin — it must pass on 1.0.1 regardless of the pin (the pin only affects the endpoint we build, not the preset's map):

```bash
cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(default_relay_map_is_stable_non_canary)'
```

Expected: PASS.

- [ ] **Step 2: Delete the pin**

Remove `STABLE_RELAY_URLS`, `stable_relay_mode()`, and the `.relay_mode(stable_relay_mode())` call in `new_with_secret()`'s builder chain (production path returns to preset defaults). Keep every OTHER `.relay_mode(RelayMode::Disabled)` (hermetic test/warm-up paths — untouched). Delete the old test if Step 1 replaced it in place.

- [ ] **Step 3: Rewrite the stale module docs**

Two doc blocks in `iroh_endpoint.rs` still narrate the 0.98 world: the module doc (~19-36, "0.98 adaptations: NodeId→EndpointId, builder takes a Preset…") and the relay-pin rationale block (~119-135, "presets::N0 hard-codes the CANARY cluster"). Rewrite both to describe the 1.0.1 reality: preset N0 defaults to the stable production relay cluster; the regression test guards it; ZEB-617's pin was retired here.

- [ ] **Step 4: Gate and commit**

```bash
cd src-tauri && cargo fmt --all \
  && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings \
  && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(iroh_endpoint) | test(relay)'
```

Expected: all green.

```bash
git add -A src/iroh_endpoint.rs && git commit -m "feat: retire ZEB-617 relay pin — iroh 1.0 defaults are the stable cluster (ZEB-619)"
```

---

### Task 3: Full-workspace sweep — all targets compile, lint, and test green

This is the ONE full sweep (Global Constraints). It compiles the ~97 integration binaries against iroh 1.0.1 for the first time — the most API-heavy is `tests/network_health_two_endpoint.rs` (`MemoryLookup`, `.address_lookup()`, `EndpointAddr::from_parts` + `TransportAddr::Ip`); all of those are name-stable in 1.0.1 per the research pass, so expect zero-to-trivial fixes.

**Files:**
- Possibly modify: any `src-tauri/tests/*.rs` or inline `#[cfg(test)]` module that trips on the new version (fix mechanically, same idiom).

**Interfaces:**
- Consumes: Tasks 1-2 committed.
- Produces: a fully green tree; ZEB-616's reconnect semantics revalidated on 1.0 (acceptance criterion 2).

- [ ] **Step 1: Compile everything**

```bash
cd src-tauri && cargo check --locked --workspace --all-targets --features test-fixtures
```

Fix any fallout mechanically; commit fixes as `fix: iroh 1.0 test-target fallout (ZEB-619)` if any.

- [ ] **Step 2: Lint + fmt**

```bash
cd src-tauri && cargo fmt --all -- --check \
  && cargo clippy --locked --workspace --all-targets --features test-fixtures --no-deps -- -D warnings
```

- [ ] **Step 3: Full test suite (commit BEFORE this; runs 10-15 min — background it and poll)**

```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: full pass (baseline was 3961 tests). PAY SPECIFIC ATTENTION to the ZEB-616 revalidation set — these must be in the passing list, named in your report:
- `zenoh_reconnect_closes_stale_connection` (the hermetic same-zid reconnect regression)
- the `zenoh_iroh_transport` registry tests (swap/evict watcher suite, ~lines 826-1000)
- `iroh_zenoh_registration_integration`, `zeb_373_dynamic_dial_integration`, `pkarr_iroh_redeem_full_integration`, `network_health_two_endpoint`

Any UNRELATED pre-existing failure (not iroh-touching): do NOT fix in this branch — report it for a follow-up ticket per the unrelated-test-failures rule.

- [ ] **Step 4: Commit any remaining changes**

```bash
git add -A && git diff --cached --quiet || git commit -m "test: full-workspace sweep green on iroh 1.0.1 (ZEB-619)"
```

(If Steps 1-2 produced no diffs there is nothing to commit — say so in the report instead of forcing an empty commit.)

---

### Task 4: e2e-harness real-transport scenarios S1/S3/S4 (acceptance criterion 3)

Real-network validation: two co-located `harmony-app serve` nodes (both on iroh 1.0.1 — co-location sidesteps the wire flag-day) exercising invite/join first-contact (S1), offline-channel reconnect catch-up (S3), restart durability (S4). First contact is racy + relay-dependent (~75-90s pkarr propagation; relays warm ~1-2 min) — allow several minutes wall-clock; scenarios poll/retry internally.

**Files:** none modified (validation only; e2e-harness has its own Cargo.lock and does NOT depend on iroh directly — it drives the binary over HTTP/WS).

**Interfaces:**
- Consumes: Task 3's green tree.
- Produces: pass/fail evidence for the three scenarios.

- [ ] **Step 1: Build the binary the harness drives**

```bash
cd src-tauri && cargo build --locked --bin harmony-app
```

- [ ] **Step 2: Run the three scenarios serially (commit-before-gate does not apply — no diffs; but background + poll, total budget ~20 min)**

```bash
cd e2e-harness && cargo nextest run --locked --features e2e --test-threads 1 \
  -E 'test(s1_invite_join_roster_convergence) | test(s3_offline_channel_reconnect_catchup) | test(s4_restart_durability)'
```

Expected: 3/3 PASS. On failure: retry the failing scenario ONCE (first-contact raciness is documented); a second failure is a real finding — capture the run artifacts (`HARMONY_E2E_KEEP=1`, `e2e-harness/target/e2e-runs/`) and report DONE_WITH_CONCERNS with the failing scenario's log tail.

---

### Task 5: Live smoke on an isolated profile (acceptance criterion 4) + PR

**Files:** none modified (validation + PR).

**Interfaces:**
- Consumes: Tasks 1-4 complete.
- Produces: smoke evidence + the open PR.

- [ ] **Step 1: Boot an isolated upgraded node and capture its log**

(Do NOT touch the fleet-koya profile — Global Constraints.)

```bash
cd src-tauri && HARMONY_PROFILE=zeb619-smoke HARMONY_DISABLE_KEYCHAIN=1 HARMONY_PASSPHRASE=zeb619 \
  RUST_LOG=info,iroh=info ./target/debug/harmony-app serve --api-port 7621 > /tmp/zeb619-smoke.log 2>&1 & \
SMOKE_PID=$!
# Wait for the API to come up (readiness poll) rather than trusting a fixed
# boot delay, THEN hold a fixed observation window for relay negotiation.
for i in $(seq 1 30); do curl -s -o /dev/null "http://127.0.0.1:7621/health" && break; sleep 2; done
sleep 60
kill "$SMOKE_PID"
```

(One shell invocation so `$!` stays valid. The poll bounds boot variance; the
60s window after readiness is for relay handshake + pkarr publish.)

- [ ] **Step 2: Assert the smoke criteria against the log**

```bash
grep -c "iroh-canary" /tmp/zeb619-smoke.log            # expected: 0
grep -c "MaxPathIdReached" /tmp/zeb619-smoke.log        # expected: 0
grep -o "[a-z0-9-]*\.relay\.n0\.iroh\.link" /tmp/zeb619-smoke.log | sort -u   # expected: >=1 stable relay host
```

If the third grep finds nothing, look for whatever relay-connection line the 1.0 logs emit (`home relay`, `relay connected`, etc.) and verify the host manually. Report the exact evidence lines.

- [ ] **Step 3: Open the PR**

Title: `ZEB-619: upgrade iroh 0.98.2 -> 1.0.1 — stable relays by default, MaxPathIdReached recovery`

Body MUST include (Linear-integration rules):
- The literal magic-word line `Closes ZEB-615` (ZEB-619 itself auto-links via the branch name).
- NO other bare `ZEB-NNN` identifiers except ZEB-619/ZEB-615 (cascade guard — refer to other tickets in prose without IDs).
- A **Fleet flag-day** section: iroh v1 is wire-frozen and CANNOT interoperate with 0.98 — after merge, every fleet machine (Koya/Ildwyn/AVALON) must rebuild before restarting its node; until then, do not restart running nodes onto mixed builds.
- MSRV note: rust-version 1.88 → 1.91 (iroh floor; toolchain pin 1.94.1 already satisfies it; CI msrv job auto-retargets).
- Evidence: gate results, ZEB-616 revalidation test names, e2e S1/S3/S4 results, smoke grep output.

```bash
gh pr create --repo zeblithic/harmony-client --head zeb-619-iroh-1-0-upgrade --title "..." --body "..."
```

(Controller note, not implementer: CodeRabbit gets its ONE review trigger at PR creation; Qodo auto-reviews; never mention-trigger Greptile.)
