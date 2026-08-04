# ZEB-864 Open-Join Shed Acceptor-Harness Regression Test — Implementation Plan

> **For agentic workers:** Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Lock in the invite acceptor's pre-auth Tier-1 shed properties (sheds
before packet decode; writes zero response bytes) with a regression test that
drives an actual shed through the real `handle_invite_handshake_inbound` over a
real localhost iroh connection.

**Architecture:** Add one builder seam (`with_open_join_conn_limiter`) to inject a
zero-cap limiter, then a new integration test that stands up a hermetic iroh
endpoint pair, drives the handler directly, and asserts shed-pre-decode +
zero-bytes (Case A) against a permissive-limiter control that reaches the read
and times out (Case B).

**Tech Stack:** Rust, `cargo nextest`, real iroh endpoints (`presets::Minimal`).

## Global Constraints

- Build/test from `src-tauri/`. Iterative gate: `scripts/test-select --context task`.
  Paste the printed `round=… bucket=…` summary line into the task report.
  Final pre-PR sweep: `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
  + `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  + `cargo fmt --all -- --check`.
- No production behavior change. The only production edit is the
  `with_open_join_conn_limiter` override builder; production never calls it and the
  default limiter/caps are untouched.
- Not a trait abstraction of `Connection` — real hermetic endpoints, mirroring
  `tests/misc/community_open_join_cross_wan_integration.rs`.
- This is a regression test for **already-correct** code: Case A/B PASS on current
  `main`. Its teeth are proven by a mutation check (Step 6), not a TDD red phase.

---

### Task 1: seam + shed/control regression tests + mutation-verified teeth

**Files:**
- Modify: `src-tauri/src/iroh_invite_acceptor.rs` (add builder seam near `with_traffic_registry` :293)
- Create: `src-tauri/tests/misc/open_join_shed_acceptor_harness_integration.rs`
- Reference (do not modify): `src-tauri/tests/misc/community_open_join_cross_wan_integration.rs`
  (`build_hermetic_endpoint` :131, `setup_two_party_open_join` acceptor construction :569,
  `bob_dial_ctx`/dial mechanics :727), `src-tauri/src/open_join_admit.rs`
  (`OpenJoinConnLimiter::with_caps` :235), `src-tauri/src/friend_intro.rs`
  (`KeyedSlidingWindow::admit` max==0 sheds :629).

**Interfaces:**
- Produces: `IrohInviteHandshakeAcceptor::<H>::with_open_join_conn_limiter(self, OpenJoinConnLimiter) -> Self`.
- Consumes: `handle_invite_handshake_inbound(&Connection) -> Result<EventId, HandshakeAcceptError>`,
  `HandshakeAcceptError::{ConnectionShed, IoTimeout{step}, ReadPrefix}`.

- [ ] **Step 1: Add the builder seam.** In `iroh_invite_acceptor.rs`, directly after
  `with_traffic_registry`, add:

  ```rust
  /// ZEB-864: override the pre-auth connection shield's limiter. Production wiring
  /// never calls this (the default `OpenJoinConnLimiter::new()` carries production
  /// caps); the acceptor-harness shed test injects a zero-cap limiter to force a
  /// deterministic shed. Builder-style so it composes with the other `with_*` seams.
  pub fn with_open_join_conn_limiter(mut self, limiter: OpenJoinConnLimiter) -> Self {
      self.open_join_conn_limiter = limiter;
      self
  }
  ```

  Verify `OpenJoinConnLimiter` is already in scope (imported at :60). Run
  `cargo check --locked --features test-fixtures` — expect clean.

- [ ] **Step 2: Scaffold the test file.** Create the new file. Copy the minimal pieces
  from the cross-WAN test (do not import its private helpers — mirror them):
  - `build_hermetic_endpoint()` (real `Endpoint::builder(presets::Minimal)` →
    `IrohEndpoint::from_endpoint_for_test`).
  - A minimal Alice `IrohInviteHandshakeAcceptor::<()>::with_config(...)` construction
    with a short `io_deadline` (e.g. 250ms) `HandshakeAcceptorConfig` and valid
    `CommunitySyncRegistry` / `DmOutbox` / `OwnerState` stubs (the shed fires before any
    of them is used except the `DmOutbox.self_owner` snapshot at :320, so they only need
    to be constructible). Reuse the exact construction the cross-WAN `setup_two_party_open_join`
    uses at :569, stripped of rendezvous/CAS.
  - A shared dialer helper `async fn dial_and_stub(bob_ep, alice_addr, alpn) -> (Connection /*bob side*/, SendStream, RecvStream)`
    that dials, `open_bi()`, writes a 1-byte stub, flushes (no `finish()`), and returns
    the streams so the caller can hold them open and later read the response.
  - Confirm the invite ALPN constant used by the acceptor (grep `ALPN` in
    `iroh_invite_acceptor.rs`); use it for the dial.
  Run `cargo check --locked --features test-fixtures --test open_join_shed_acceptor_harness_integration`.

- [ ] **Step 3: Write Case A (shed).** `#[tokio::test] async fn open_join_shed_returns_connection_shed_pre_decode_zero_bytes()`:
  build Alice with `.with_open_join_conn_limiter(OpenJoinConnLimiter::with_caps(0, 60_000))`;
  spawn Bob's dial-and-stub; on Alice's endpoint `accept().await` → `Connection`; call
  `acceptor.handle_invite_handshake_inbound(&conn).await`.
  Assert:
  ```rust
  assert!(matches!(res, Err(HandshakeAcceptError::ConnectionShed)),
      "zero-cap limiter must shed before decode, got {res:?}");
  ```
  Then read Bob's recv half and assert **zero data bytes** (robust to reset-vs-EOF):
  ```rust
  // read_to_end errors on reset; Ok(v) on clean EOF. Either way: no response content.
  let got = bob_recv.read_to_end(64).await;
  let data_len = got.map(|v| v.len()).unwrap_or(0);
  assert_eq!(data_len, 0, "shed must write zero response bytes (no oracle)");
  ```
  Run: `scripts/test-select --context task -E 'test(open_join_shed)'` (or the file
  filter). Expect **PASS** (existing correct behavior).

- [ ] **Step 4: Write Case B (control).** `#[tokio::test] async fn open_join_permissive_limiter_reaches_length_prefix_read()`:
  identical setup and **identical dialer**, but `.with_open_join_conn_limiter(OpenJoinConnLimiter::new())`
  (permissive). Assert:
  ```rust
  assert!(matches!(res,
      Err(HandshakeAcceptError::IoTimeout { step: "read length-prefix", .. })),
      "permissive limiter must pass the gate and time out at the length-prefix read, got {res:?}");
  ```
  Run the file filter. Expect **PASS**. (If the observed error is `ReadPrefix` instead
  of `IoTimeout` — e.g. the stub triggers EOF rather than a stall — accept a read-stage
  error that is NOT `ConnectionShed`: `assert!(!matches!(res, Err(ConnectionShed) | Ok(_)))`
  plus a comment naming the observed variant. The load-bearing property is "reached the
  read stage past the gate.")

- [ ] **Step 5: Run gates.** `scripts/test-select --context task`;
  `cargo fmt --all`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`.
  All clean.

- [ ] **Step 6: Mutation check (prove the teeth).** Temporarily edit
  `handle_invite_handshake_inbound`: move the `admit_connection` shed block to *after* the
  length-prefix read (or comment it out). Rebuild + run Case A — it MUST now FAIL (Case A
  no longer sees `ConnectionShed`). Record the observed failure in the task report.
  **Revert the mutation** (`git checkout -- src/iroh_invite_acceptor.rs` keeping the Step-1
  seam — re-apply the seam if the checkout drops it, or stash the seam first). Re-run Case
  A — PASS. This proves the regression test is non-vacuous.

- [ ] **Step 7: Full CI-parity sweep.**
  `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
  + clippy `--all-targets` + `cargo fmt --all -- --check`. All green. `git add` the new
  test file before the sweep (untracked files are invisible to test-select).

- [ ] **Step 8: Commit.** `ZEB-864: acceptor-harness regression test for open-join Tier-1 shed`.
