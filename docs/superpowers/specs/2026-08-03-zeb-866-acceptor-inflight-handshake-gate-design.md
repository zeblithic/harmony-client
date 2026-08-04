# ZEB-866: Pre-`accept_bi` in-flight handshake gate — Design

**Ticket:** ZEB-866 — Acceptor-family follow-up: bound pre-`accept_bi` connection
slowloris (gate/semaphore before stream accept) across invite/friend/pex acceptors.

**Status:** Design approved 2026-08-03.

## Problem

Each inbound handshake connection is handled by a task the zenoh accept loop
spawns per connection (`zenoh_iroh_transport.rs:591`). For a handshake ALPN
(`harmony/handshake/v1`, `harmony/friend/v1`, `harmony/friend-pex/v1`) that task
calls `dispatcher.handle_connection(conn)` (`:721`, fast path; `:556`, boot-drain
path), which routes through `MultiplexHandshakeDispatcher::handle_connection`
(`iroh_friend_acceptor.rs:2577`) to the selected acceptor's `handle_connection`,
whose **first** await is `conn.accept_bi()` under `io_deadline`.

`io_deadline` defaults to **30 s** (`DEFAULT_ACCEPTOR_IO_DEADLINE_MS`,
`DEFAULT_FRIEND_IO_DEADLINE_MS`). A peer that opens a connection but **withholds
the bidirectional stream** pins that handler task for up to 30 s. The per-source
Tier-1 `admit_connection` shield runs *after* `accept_bi` returns (invite
`:344→:386`, friend `:2086→:2113`, pex `:536→:648`), so it never sees a
withheld-stream connection. There is **no pre-`accept_bi` bound**: both a
single source and a Sybil fan-out can pile up unbounded concurrent handler
tasks (each ≈ one spawned task + one `Connection` + QUIC state, for ≤ 30 s).

**Not a B7 regression.** B7's target — unbounded pre-consent ed25519 crypto —
is fully closed (the shield is before decode/crypto). This is a distinct,
pre-existing, `io_deadline`-bounded resource concern shared by all three
acceptors.

## Key realization: the resource is concurrency, not rate

The existing Tier-1 shields (and ZEB-865's aggregate ceiling) are sliding-window
**rate** limiters (N admissions per 60 s window). What is unbounded here is
**concurrent in-flight** handler-task occupancy. A rate limiter does not cap
concurrency — 8 stalled connections from one source, opened slowly enough to
stay under any rate, still pin 8 tasks for 30 s each.

The correct tool is a **concurrency limiter**: a permit acquired *before*
delegating to the inner acceptor (before `accept_bi`), **held across the whole
handshake** via an RAII guard, and released when the handler returns (success,
error, or shed). When no permit is available, the connection is shed.

## Placement: the single multiplex chokepoint

All three acceptors route through one `MultiplexHandshakeDispatcher::handle_connection`
(`:2577`), a thin router that owns the `Connection` and re-reads `conn.alpn()`
before delegating. `conn.remote_id()` is available here, *before* any inner
`accept_bi` (ZEB-616 already relies on this exact property in the zenoh accept
loop, `:607-616`).

Placing the gate here means:

- **One implementation** covers invite/friend/pex uniformly (the ticket's
  consistency requirement) — no triplication.
- **Both entry paths** (fast path `:721`, boot-drain `:556`) are covered for
  free, since both call `dispatcher.handle_connection`.
- The inner acceptors and their post-`accept_bi` shields are **untouched** —
  B7's per-source rate shield and its anti-lockout property are preserved
  exactly.

## Component 1 — `InFlightHandshakeGate` (new module `inflight_handshake_gate.rs`)

A two-tier concurrency limiter over connection `remote_id`s.

```rust
/// Per-source concurrent in-flight cap: how many handshakes one `remote_id`
/// may have in flight at once. Well above honest use (1-3), tightly bounds a
/// single-source stalled flood.
pub const HANDSHAKE_INFLIGHT_PER_SOURCE_MAX: usize = 8;

/// Node-wide concurrent in-flight cap: total handshakes in flight across all
/// sources. ~10x a heavy legitimate join surge (honest handshakes hold a slot
/// < 1 s), caps a Sybil fan-out (uncapped: attacker_rate x 30 s io_deadline).
pub const HANDSHAKE_INFLIGHT_GLOBAL_MAX: usize = 1024;

pub(crate) struct InFlightHandshakeGate {
    inner: std::sync::Mutex<InFlightState>,
    per_source_max: usize,
    global_max: usize,
}

struct InFlightState {
    global: usize,
    per_source: std::collections::HashMap<[u8; 32], usize>,
}

impl InFlightHandshakeGate {
    /// The only constructor — takes both caps explicitly. Production callers
    /// pass the constants (see the dispatcher's `new()`); tests pass tiny caps.
    /// Exposing only this (no no-arg `new()`) sidesteps
    /// `clippy::new_without_default`.
    pub(crate) fn with_caps(per_source_max: usize, global_max: usize) -> Self { /* zeroed state */ }

    /// Reserve one in-flight slot for `source`. Returns a guard that releases
    /// the slot on drop, or `None` if the global OR per-source ceiling is
    /// already reached (→ caller sheds). Checks global first, then per-source.
    pub(crate) fn try_acquire(self: &std::sync::Arc<Self>, source: [u8; 32]) -> Option<InFlightGuard>;
}

pub(crate) struct InFlightGuard {
    gate: std::sync::Arc<InFlightHandshakeGate>,
    source: [u8; 32],
}

impl Drop for InFlightGuard {
    // decrement global (saturating) + per_source[source]; remove the entry at zero.
}
```

**Two deliberate design choices:**

1. **`std::sync::Mutex`, not `tokio::sync::Mutex`.** `Drop` cannot be `async`,
   and the critical section is O(1) with no `.await` — a sync lock is both
   correct and necessary. The lock is never held across an await, so it cannot
   cause an async stall.

2. **The per-source map is self-bounding.** Every per-source entry also counts
   toward `global`, and entries are removed at zero, so
   `per_source.len() <= global <= global_max = 1024`. Unlike ZEB-865's
   `KeyedSlidingWindow` (which needs `MAX_WINDOW_KEYS` because rate-window
   entries linger after the request completes), concurrency entries vanish on
   completion — **no separate key cap is needed**. This is the structural
   reason a concurrency limiter is simpler here than a rate limiter would be.

**`try_acquire` / `Drop` pairing (second-order correctness):** `try_acquire`
increments `global` and `per_source[source]`; the returned guard carries
`source`; `Drop` decrements both and removes the per-source entry at zero.
`saturating_sub` on `global` is defensive against underflow. Multiple guards
for the same source stack the count; each `Drop` decrements; the entry is
removed only when the last guard drops. Balanced by construction.

## Component 2 — chokepoint wiring (`MultiplexHandshakeDispatcher`)

Add one field and consult the gate before delegating:

```rust
pub struct MultiplexHandshakeDispatcher {
    invite: Arc<dyn IrohHandshakeDispatcher>,
    friend: Arc<dyn IrohHandshakeDispatcher>,
    pex: Arc<dyn IrohHandshakeDispatcher>,
    gate: Arc<InFlightHandshakeGate>,           // new
}

async fn handle_connection(&self, conn: Connection) {
    let source = *conn.remote_id().as_bytes();          // available pre-accept_bi
    let _guard = match self.gate.try_acquire(source) {
        Some(g) => g,
        None => {
            tracing::warn!(
                remote_id = ?conn.remote_id(),
                "ZEB-866: in-flight handshake gate shed (per-source or global cap)"
            );
            // Clean CONNECTION_CLOSE, zero response-stream bytes — the same
            // benign, retryable shape as the Tier-1 post-accept_bi shed.
            // Mirrors the boot-queue-aged-out close (zenoh_iroh_transport.rs:544).
            conn.close(0u32.into(), b"zeb866-inflight-cap");
            return;
        }
    };
    self.select_for_alpn(conn.alpn()).handle_connection(conn).await;
    // _guard drops here → global + per-source slot released
}
```

**Constructor seam.** `new(invite, friend, pex)` builds the gate via
`InFlightHandshakeGate::with_caps(HANDSHAKE_INFLIGHT_PER_SOURCE_MAX,
HANDSHAKE_INFLIGHT_GLOBAL_MAX)` — **no call-site change** (the only production
caller, `lib.rs:10462`, keeps its three-arg form). A `with_gate_caps(invite,
friend, pex, per_source_max, global_max)` test seam takes tiny caps, mirroring
ZEB-865's `new()` / `with_caps()`.

## Data flow

1. Accept loop spawns a task per connection; handshake ALPN → `dispatcher.handle_connection(conn)`.
2. `MultiplexHandshakeDispatcher::handle_connection` reads `remote_id`, calls `gate.try_acquire`.
3. **Admit** → hold guard, delegate to inner acceptor (`accept_bi` + Tier-1 shield + handshake), guard drops on return.
4. **Shed** (global or per-source at cap) → `conn.close`, return; no delegation, no `accept_bi`, zero response bytes.

## Error handling / shed semantics

Shed is not an error path — it is a benign, retryable close, identical in
observable shape to the existing Tier-1 shield's shed (clean CONNECTION_CLOSE,
no response-stream bytes). No new error type; the inner acceptors' error
handling is unchanged. A shed connection never reaches `accept_bi`, so it never
occupies a handler slot beyond the O(1) gate check + close.

## Anti-lockout (B7 preservation)

The per-source tier is what preserves B7's anti-lockout property under a global
cap: because one `remote_id` can hold at most `HANDSHAKE_INFLIGHT_PER_SOURCE_MAX
= 8` global slots, a single source cannot monopolize the 1024-slot global pool
and lock out honest joiners. A global cap *alone* would reintroduce exactly the
global-lockout DoS B7 removed — which is why per-source is mandatory, not
optional.

## Testing

Following the established `MultiplexHandshakeDispatcher` test pattern
(`iroh_friend_acceptor.rs:2586`, `:4314`), which unit-tests decision logic with
stub dispatchers — a live `iroh::endpoint::Connection` cannot be constructed
in-process (comment at `:2561`).

**Primitive unit tests (`inflight_handshake_gate.rs`):**

1. Per-source cap: `with_caps(2, 100)` — source A acquires 2, the 3rd returns
   `None`; drop one guard, next acquire succeeds.
2. Global cap: `with_caps(8, 3)` — 3 distinct sources each acquire 1, a 4th
   distinct source returns `None` even though it is under its per-source cap.
3. Guard release restores state: acquire several, drop all → `global == 0` and
   the per-source map is empty (self-bounding).
4. **Anti-lockout:** `with_caps(2, 100)` — source A holds 2 (at its per-source
   cap); source B still acquires. One source does not lock out others.

**The ticket's explicit requirement — "a stalled/withheld-stream flood from one
source is shed before it can occupy unbounded handler slots" — is satisfied by
tests 1 + 4 together:** test 1 proves a single source's over-cap connection is
refused by `try_acquire` (returns `None`) *before* `handle_connection` delegates
to the inner acceptor (the guard-or-`return` wiring), i.e. before it reaches
`accept_bi` and occupies a handler slot; test 4 proves that shed does not lock
out other sources. The gate primitive is the testable unit — the dispatcher's
`handle_connection` cannot be driven in-process (no constructible `Connection`),
so its one-line guard-or-shed wiring is review-verified, and the real-iroh
loopback assertion is deferred to ZEB-870 (below).

## Scope boundary — loopback wiring test deferred to ZEB-870

The end-to-end loopback assertion (a real dialer sees the (K+1)th connection
closed with zero bytes over iroh) belongs to **ZEB-870**, which is specifically
about adding dispatcher-path (`handle_connection`) coverage over real iroh.
Rather than build that harness twice, ZEB-866 keeps its tests at the
unit/decision level, and ZEB-870's scope note is expanded to also assert the
in-flight-gate shed alongside the `ConnectionShed` outcome. This keeps this PR
focused and its tests deterministic.

## Files

- **Create:** `src-tauri/src/inflight_handshake_gate.rs` — the primitive + its unit tests.
- **Modify:** `src-tauri/src/lib.rs` — `pub mod inflight_handshake_gate;` declaration.
- **Modify:** `src-tauri/src/iroh_friend_acceptor.rs` — `gate` field, `new()`
  constructs it, `with_gate_caps` test seam, `handle_connection` wiring, +
  decision-level tests.
- **Update (follow-up):** ZEB-870's description — expand scope note to cover the gate shed.

## Global constraints

- Rust; build/test from `src-tauri/`.
- CI parity gates: `cargo fmt --all -- --check`; `cargo clippy --locked
  --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo
  nextest run --locked --workspace --all-targets --features test-fixtures`.
- No new dependencies. No production behavior change beyond the gate (inner
  acceptors and their shields untouched).
