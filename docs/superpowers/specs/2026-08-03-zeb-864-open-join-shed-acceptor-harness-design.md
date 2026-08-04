# ZEB-864: Acceptor-harness regression test for the open-join Tier-1 shed path

**Ticket:** ZEB-864 (ZEB-853 B7 follow-up).
**Scope:** `src-tauri/src/iroh_invite_acceptor.rs` (one builder seam) +
`src-tauri/tests/misc/open_join_shed_acceptor_harness_integration.rs` (new test).
Test-first hardening — no production behavior change.

## Problem

ZEB-853 B7 added a pre-auth Tier-1 connection shield to the invite acceptor:
`handle_invite_handshake_inbound` calls `open_join_conn_limiter.admit_connection(...)`
(iroh_invite_acceptor.rs:374-384) and, on shed, returns `HandshakeAcceptError::ConnectionShed`
**before** the length-prefix read at :387. Two properties are load-bearing:

1. **Pre-decode shed.** The gate sits *after* `accept_bi()` (:334) but *before* any
   stream read / decode / crypto. Its whole purpose is to bound pre-consent ed25519
   work by a caller who holds only the public open-invite link.
2. **Oracle-safe zero-byte shed.** On a shed the acceptor logs and writes **nothing**,
   returning `ConnectionShed`, which `handle_connection` treats exactly like the benign
   no-response outcomes (`CountersignTimeout`, `CommunityNotFound`): warn-log +
   `conn.closed()`, zero response bytes. A shed must therefore be byte-indistinguishable
   from those benign outcomes — no rejection-content oracle.

Both are **correct-by-inspection only**. Current coverage:
- The limiter primitive (`OpenJoinConnLimiter` / `KeyedSlidingWindow`) is unit-tested
  directly (`open_join_tier1_sheds_one_source_not_others`, `open_join_rate_limit_is_per_source`).
- The 8 cross-WAN integration tests run production caps (40/1h) that an honest join
  never trips — they prove the shield doesn't break the honest path, but never drive a
  shed through the real `handle_invite_handshake_inbound`.

**Gap:** a future edit that (a) moves the gate below a stream read/decode/crypto, or
(b) writes distinguishing bytes on the shed path, would go uncaught.

## Fix

Add a regression test that drives an actual shed through the real acceptor over a real
localhost iroh connection, plus the minimal production seam to inject a shedding limiter.

### 1. Production seam (the only production change)

`handle_invite_handshake_inbound` takes a concrete `iroh::endpoint::Connection` — not a
trait — so the test uses a **real hermetic iroh endpoint pair**, not a fake. Abstracting
`Connection` behind a trait would be an invasive cross-acceptor refactor and is a
non-goal. What the test needs is a way to install a zero-cap limiter on the acceptor.

Add a builder mirroring the existing `with_traffic_registry` (:293):

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

`OpenJoinConnLimiter::with_caps(0, window_ms)` is an always-shed limiter:
`KeyedSlidingWindow::admit` returns `false` immediately when `max == 0`
(friend_intro.rs:629-631), so the first `admit_connection` sheds — deterministic, no
timing dependence.

### 2. The test file

New integration test `tests/misc/open_join_shed_acceptor_harness_integration.rs`,
co-located with the existing open-join cross-WAN test. It stands up two hermetic
endpoints (mirroring `build_hermetic_endpoint()` — real `Endpoint::builder(presets::Minimal)`),
and constructs Alice's `IrohInviteHandshakeAcceptor::<()>::with_config(...)` with the same
minimal stubs the cross-WAN `setup_two_party_open_join` uses. The shed fires before any
community / CAS / membership logic, so none of that fixture is needed — only a valid
`DmOutbox` (its `self_owner` is snapshotted at :320, before the gate) and the endpoints.

**Shared dialer (the sole variable between cases is the limiter).** Both cases use one
dialer helper: Bob dials Alice on the invite ALPN, `open_bi()`, writes a **1-byte stub**,
and **holds the stream open** (does not `finish()`) until the acceptor's result is
observed. Two wire-protocol facts drive this shape:

- A client-opened bidi stream is not signaled to the server until the client writes (or
  finishes) — a truly silent stream would leave Alice blocked in `accept_bi()` itself,
  never reaching the gate. The 1-byte stub guarantees `accept_bi()` returns.
- The stub is a *partial* (< 4-byte) length prefix and the stream stays open, so a
  permissive acceptor that reaches the length-prefix read (`read_exact(&mut [0u8; 4])`)
  blocks for the missing bytes and hits `io_deadline` — a clean, deterministic
  `IoTimeout { step: "read length-prefix" }` for the control.

**Case A — shed (zero-cap limiter):**
1. Alice acceptor built with `.with_open_join_conn_limiter(OpenJoinConnLimiter::with_caps(0, W))`.
2. Shared dialer (1-byte stub, held open).
3. Alice `accept()`s the connection and calls `handle_invite_handshake_inbound(&conn)`.
4. **Assert** the result is exactly `Err(HandshakeAcceptError::ConnectionShed)` — and
   NOT `IoTimeout { step: "read length-prefix", .. }` nor `ReadPrefix(..)`. Bob sent only
   a 1-byte stub, so the *only* way Alice returns `ConnectionShed` instead of a
   read/timeout error is if she shed **before** attempting the length-prefix read →
   proves pre-decode.
5. **Assert** Bob receives **zero application/data bytes** in response. Driven directly
   (not via `handle_connection`), the handler drops its send half on return, so Bob's
   read observes either a clean 0-byte EOF or a stream reset — never response *content*.
   The assertion is "zero data bytes received" (robust to reset-vs-EOF), which is the
   oracle property: byte-identical to the benign no-response outcomes.

**Case B — control (permissive-cap limiter), the regression teeth:**
1. Same construction, **same dialer**, but a permissive limiter (`OpenJoinConnLimiter::new()`).
2. Alice now passes the gate and blocks on the length-prefix read (only 1 of 4 bytes
   available, stream held open), returning
   `Err(HandshakeAcceptError::IoTimeout { step: "read length-prefix", .. })` once the
   short `io_deadline` elapses.
3. **Assert** that error. Since the dialer is identical to Case A, the different outcome
   is attributable *only* to the limiter cap — proving the `ConnectionShed` in Case A is
   caused by the gate at its pre-decode position, not incidentally by the stream shape.
   Use a short `io_deadline` in the acceptor config so the control times out fast.

Case B is deliberately one step beyond the ticket's literal ask (shed assertion only):
without it, the "before decode" claim is only incidentally true, and an edit that moves
the gate below the read could still spuriously pass Case A.

## How this catches the named regressions

- **Gate moved below a read/decode/crypto:** in Case A the acceptor would now attempt the
  length-prefix read on Bob's 1-byte-stub stream and return a read/timeout error
  (`IoTimeout`/`ReadPrefix`) instead of `ConnectionShed` → Case A fails.
- **Distinguishing bytes written on the shed path:** Bob's recv read in Case A returns
  > 0 bytes → the zero-bytes assertion fails.

## Non-goals / invariants preserved

- No production behavior change. The only production edit is an override builder that
  production never calls; the default limiter and caps are untouched.
- Not a trait abstraction of `Connection` — real endpoints, mirroring the established
  cross-WAN test pattern.
- Timing side-channel (shed returns fast vs. `CountersignTimeout` returns slow) is
  explicitly out of scope — this test asserts response-*byte* equivalence, not
  response-*latency* equivalence, matching the shield's documented guarantee.
- The friend/PEX acceptors are untouched (their shields are audited separately; the
  sibling ZEB-866 covers the shared pre-`accept_bi` gating).
