# ZEB-912 Step 2: Router-Mode Knob + Severed-Pair Proof Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Env-gated router-mode zenoh sessions + a 3-node e2e proving channel delivery across a severed pair, per the spec (`docs/superpowers/specs/2026-08-13-zeb912-router-mode-step2-design.md`).

**Architecture:** Four additive seams — a mode knob into the existing config build, a mode param on the listen-endpoint merge, a `peers_zid∪routers_zid` helper at two existing sites, a test-only link denylist gating dial+accept — plus `nodeId` on `/v1/status` and e2e scenario s14.

**Tech Stack:** Rust (src-tauri), zenoh 1.9.0 pinned, e2e-harness crate.

## Global Constraints

- Production behavior with all new env vars unset must be byte-identical except the `timestamping/enabled=false` pin (a no-op vs today's peer default).
- Env knob names exactly: `HARMONY_ZENOH_MODE`, `HARMONY_TEST_ZENOH_DENYLIST`.
- Cargo gates from `src-tauri/`: fmt --check, clippy `--locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `scripts/test-select --context task` from repo root per task; full sweep pre-PR.
- No frontend/TS changes (StatusDto is API-only; harness reads JSON).
- Commit after each task with the standard trailers.

---

### Task 1: mode knob + timestamping pin + mode-aware listen merge

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (config build ~1284-1440; new `zenoh_session_mode()` near `hermetic_zenoh_config`)
- Modify: `src-tauri/src/iroh_zenoh_registration.rs` (`merge_iroh_listen_endpoints` + tests)

**Interfaces:**
- Produces: `pub(crate) fn zenoh_session_mode() -> &'static str` (consumed by config build + merge call site); `merge_iroh_listen_endpoints(current_json: Option<&str>, self_loc: &str, mode: &str) -> String`.

- [ ] **Step 1: failing tests.** In `iroh_zenoh_registration.rs` tests: update existing calls with `, "peer"`; add `merge_object_form_router_mode_appends_router_key` asserting the locator lands under `"router"` (and `"peer"` list untouched) for `{"router":["tcp/[::]:7447"],"peer":["tcp/[::]:0"]}`. In `event_loop.rs` add `zeb912_mode_knob_tests`: `zenoh_session_mode` unset→`"peer"`, `"router"`→`"router"`, `" router "`→`"router"`, `"Router"`→`"peer"`, `""`→`"peer"` (serialize env mutation with a mutex like existing env tests); plus a zeb616-style key-validity test for `mode` and `timestamping/enabled`.
- [ ] **Step 2: run — expect compile fail** (`merge` arity) + new-test fails: `scripts/test-select --dry-run` not needed; run `cargo nextest run --locked --features test-fixtures -E 'test(zeb912) or test(merge_)' --no-fail-fast` from src-tauri.
- [ ] **Step 3: implement.** `zenoh_session_mode()` per spec §2a verbatim. Config build: after the scouting block insert
  ```rust
  let mode = crate::event_loop::zenoh_session_mode(); // (local fn; path per module layout)
  if let Err(e) = config.insert_json5("mode", &format!("\"{mode}\"")) {
      tracing::warn!("zenoh config: failed to set mode={mode}: {e}");
  }
  // ZEB-912: pin timestamping so router mode can't silently flip it to true
  // (router default = true; peer default = false — wire-visible HLC stamps).
  if let Err(e) = config.insert_json5("timestamping/enabled", "false") {
      tracing::warn!("zenoh config: failed to pin timestamping/enabled: {e}");
  }
  ```
  Merge fn: add `mode: &str` param; object arm becomes `map.entry(mode)`; doc comment updated (drop "we always run in peer mode"). Call site (`event_loop.rs` ~1397-1415) passes the mode local.
- [ ] **Step 4: green + gates.** Same nextest filter passes; `cargo fmt --all`; clippy per Global Constraints; `scripts/test-select --context task`.
- [ ] **Step 5: commit** `ZEB-912: HARMONY_ZENOH_MODE knob, timestamping pin, mode-aware listen-endpoint merge`.

### Task 2: HARMONY_TEST_ZENOH_DENYLIST (dial + accept gates)

**Files:**
- Modify: `src-tauri/src/iroh_dial_driver.rs` (parse helper + dial gate + tests)
- Modify: `src-tauri/src/zenoh_iroh_transport.rs` (accept gate)

**Interfaces:**
- Produces: `pub(crate) fn zenoh_test_denylist() -> &'static std::collections::HashSet<[u8; 32]>` and `pub(crate) fn is_zenoh_test_denied(node_id: &[u8; 32]) -> bool` in `iroh_dial_driver`; consumed by the accept loop.

- [ ] **Step 1: failing tests** in `iroh_dial_driver.rs`: `denylist_parses_valid_and_skips_junk` (env `"<64hex>,notahex,,<64HEX-uppercase>"` → 2 entries, uppercase normalized — use a fresh parse fn `parse_zenoh_denylist(&str)` so tests don't fight the OnceLock), `denylist_empty_when_unset`. Dial-gate test: construct the locator for a denied id and assert `RuntimePeerDialer::dial` errs WITHOUT a runtime call — factor the check into `deny_check_from_locator(locator: &str) -> Option<[u8;32]>`-style pure fn tested directly (parses `iroh/<hex>`, returns the id when denied via an injected set).
- [ ] **Step 2: run — expect fail** (`cargo nextest ... -E 'test(denylist)'`).
- [ ] **Step 3: implement.** Pure `parse_zenoh_denylist(raw: &str) -> HashSet<[u8;32]>` (split ',', trim, lowercase, hex-decode 32 bytes, warn+skip bad); `zenoh_test_denylist()` = `OnceLock` over `env::var("HARMONY_TEST_ZENOH_DENYLIST")`; `is_zenoh_test_denied` fast-paths empty set. Dial gate at the top of `RuntimePeerDialer::dial`: parse the `iroh/<hex>` locator's id; on hit `tracing::info!(peer=%hex, "ZEB-912 test denylist: refusing dial")` + `Err`. Accept gate in the accept loop immediately after `let peer_id = conn.remote_id();` (`zenoh_iroh_transport.rs:616-617`), BEFORE `swap_zenoh_conn`/`mark_supervisor_connected`:
  ```rust
  // ZEB-912: test-only sever seam. Reject BEFORE the registry swap so a
  // denied peer never reaches mark_supervisor_connected (which would cancel
  // the remote's dialing and half-form the link).
  if crate::iroh_dial_driver::is_zenoh_test_denied(peer_id.as_bytes()) {
      tracing::info!(peer = %peer_id, "ZEB-912 test denylist: rejecting inbound");
      conn.close(0u32.into(), b"zeb912-test-denylist");
      return; // per-connection task; mirrors the ALPN-mismatch arm's exit shape
  }
  ```
  (Match the surrounding control flow — if the ALPN arm uses `continue`/return-from-spawned-task, mirror it.)
- [ ] **Step 4: green + gates** (as Task 1 Step 4).
- [ ] **Step 5: commit** `ZEB-912: HARMONY_TEST_ZENOH_DENYLIST link-layer sever seam (dial + accept)`.

### Task 3: direct-link zid union + status nodeId

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (`direct_link_zids` helper; swap at ~3843-3851 eager init and ~4581-4590 refresh)
- Modify: `src-tauri/src/lib.rs` (`node_id_hex_for_status()` near `owner_id_hex_for_status` ~1817)
- Modify: `src-tauri/src/api/mod.rs` (StatusDto + handler)

**Interfaces:**
- Produces: `async fn direct_link_zids(session: &zenoh::Session) -> HashSet<String>` (spec §2c verbatim); `NodeState::node_id_hex_for_status(&self) -> Option<String>`; StatusDto field `node_id: Option<String>` → JSON `nodeId`.

- [ ] **Step 1: failing test.** API-layer: extend the existing status DTO serialization test (or add one following the module's pattern) pinning `"nodeId"` presence/absence: `Some("ab"*32)` serializes to `"nodeId":"abab…"`, `None` → field skipped or null per existing DTO conventions (match `owner_id`'s treatment). For `direct_link_zids`, no session-free unit test is practical — covered by e2e; the refresh-site refactor keeps `detect_up_edges` inputs identical in shape (existing ZEB-622 tests keep passing untouched: that IS the regression check).
- [ ] **Step 2: run — expect fail** on the DTO test.
- [ ] **Step 3: implement.** Helper per spec; eager-init site becomes `direct_link_zids(&session).await`; refresh site collects `Vec<String>` from the helper's set for `detect_up_edges` (order-independent — verify by reading `detect_up_edges` before assuming; if it needs Vec, `into_iter().collect()`). `node_id_hex_for_status`: mirror `owner_id_hex_for_status`'s guard; source the running node's iroh endpoint (same handle `lib.rs:73447` reads: `hex::encode(ep.node_id().as_bytes())`); `None` when not running. Thread into `status_handler` alongside the existing tuple.
- [ ] **Step 4: green + gates.**
- [ ] **Step 5: commit** `ZEB-912: direct-link zid union (routers_zid) + /v1/status nodeId`.

### Task 4: e2e s14 severed-pair proof (+ harness helpers)

**Files:**
- Modify: `e2e-harness/src/node.rs` (`node_id()` via `/v1/status`; `stderr_log_contains(needle)` helper if none exists)
- Modify: `e2e-harness/tests/e2e_two_node.rs` (s14)

**Interfaces:**
- Consumes: `nodeId` from Task 3; env knobs from Tasks 1-2.
- Produces: `async fn s14_router_mode_severed_pair_delivery()`.

- [ ] **Step 1: write s14** per spec §2f, cribbing s9 (`e2e_two_node.rs:2189-2309`): spawn A (router env) → mint → `a.node_id()`; spawn B (router env) → mint; spawn C (router env + `HARMONY_TEST_ZENOH_DENYLIST=<a_id>`) → mint. A creates community + invite for B; B `poll_join_iroh`; B generates invite for C; C `poll_join_iroh`. Assert per spec: 3-way roster on all nodes (120s), channel from A converges on C non-syncing (90s), A→C and C→A channel messages (WS + read-back, 60s), and C's stderr contains `ZEB-912 test denylist: rejecting inbound` OR A's contains `refusing dial` (positive sever evidence; poll the log with a short budget after delivery lands).
- [ ] **Step 2: build binary + run s14.** `cd src-tauri && cargo build --bin harmony-app`; then `cd e2e-harness && cargo nextest run --features e2e -E 'test(s14)' --test-threads 1` (budget: expect ~2-4 min; relay-cold outliers allowed one retry).
- [ ] **Step 3: sanity — control run.** Re-run s14 with the denylist line commented?? NO — don't weaken the committed test. Instead assert in-test that the deny log actually fired (Step 1 already does; that is the control for "the sever engaged").
- [ ] **Step 4: gates.** Harness crate: `cargo fmt --all -- --check` (harness is a separate crate — run fmt there too), clippy for e2e-harness if the crate is clippy-clean today (match existing practice; do not introduce a new gate), plus src-tauri full task gate.
- [ ] **Step 5: commit** `ZEB-912: e2e s14 — router-mode severed-pair delivery proof`.

### Pre-PR gate (after Task 4)

- [ ] `cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cargo clippy --locked --lib --bins --no-deps -- -D warnings`
- [ ] Full sweep: `cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast`
- [ ] `git status` clean; push branch; PR (Closes nothing — ZEB-912 stays open for R4-feeding step 3; PR references the ticket); fire `@coderabbitai review` ONCE (substantive code PR).
- [ ] Record the s14 run output on ZEB-912.
