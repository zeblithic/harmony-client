# ZEB-866 Pre-`accept_bi` In-Flight Handshake Gate — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bound *concurrent in-flight* inbound handshakes (per-source + node-wide) at the single `MultiplexHandshakeDispatcher` chokepoint, shedding stalled/withheld-stream connections *before* they reach `accept_bi` and pin a handler task.

**Architecture:** A new `inflight_handshake_gate` module holds a two-tier concurrency limiter (`InFlightHandshakeGate`) that hands out RAII `InFlightGuard`s. `MultiplexHandshakeDispatcher::handle_connection` reads `conn.remote_id()` (available pre-`accept_bi`), `try_acquire`s a slot, holds the guard across delegation to the inner acceptor, and sheds (clean CONNECTION_CLOSE, zero bytes) when at cap. Inner acceptors and their post-`accept_bi` rate shields are untouched.

**Tech Stack:** Rust; `std::sync::Mutex` (Drop-safe RAII, no async in the critical section); `std::collections::HashMap`; iroh `Connection`.

## Global Constraints

- Build/test from `src-tauri/`.
- CI-parity gates: `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.
- **Do not pipe gate commands** (`| tail`, `| head`) — a pipe masks the real exit status. Read the command's own exit.
- Per-source cap constant: `HANDSHAKE_INFLIGHT_PER_SOURCE_MAX: usize = 8`.
- Global cap constant: `HANDSHAKE_INFLIGHT_GLOBAL_MAX: usize = 1024`.
- No new dependencies. No production behavior change beyond the gate (inner acceptors + their shields untouched).
- Shed close reason: `conn.close(0u32.into(), b"zeb866-inflight-cap")` — clean CONNECTION_CLOSE, zero response-stream bytes.

---

### Task 1: `InFlightHandshakeGate` concurrency primitive (new module)

**Files:**
- Create: `src-tauri/src/inflight_handshake_gate.rs`
- Modify: `src-tauri/src/lib.rs` — add `pub mod inflight_handshake_gate;` among the module declarations (alphabetically after `pub mod friend_intro;` at line 197, before the `iroh_*` block).

**Interfaces:**
- Produces (used by Task 2):
  - `pub const HANDSHAKE_INFLIGHT_PER_SOURCE_MAX: usize = 8;`
  - `pub const HANDSHAKE_INFLIGHT_GLOBAL_MAX: usize = 1024;`
  - `pub(crate) struct InFlightHandshakeGate` with `pub(crate) fn with_caps(per_source_max: usize, global_max: usize) -> Self` and `pub(crate) fn try_acquire(self: &Arc<Self>, source: [u8; 32]) -> Option<InFlightGuard>`.
  - `pub(crate) struct InFlightGuard` (RAII; releases on `Drop`).

- [ ] **Step 1: Create the module with the full implementation.**

Create `src-tauri/src/inflight_handshake_gate.rs`:

```rust
//! ZEB-866: a bounded *concurrency* limiter for in-flight inbound handshakes.
//!
//! Unlike the sliding-window *rate* limiters guarding the post-`accept_bi`
//! path (`open_join_admit`, `friend_intro::KeyedSlidingWindow`), this bounds
//! how many handshakes are *concurrently in flight*. A permit is acquired at
//! the `MultiplexHandshakeDispatcher` chokepoint — before delegating to an
//! inner acceptor, and therefore before that acceptor's first `accept_bi`
//! await — and held, via an RAII [`InFlightGuard`], across the whole
//! handshake. This caps handler-task occupancy against a peer that opens a
//! connection and withholds its bidirectional stream (slowloris) for up to the
//! 30 s `io_deadline`.
//!
//! Two tiers:
//! - **global** — total concurrent in-flight handshakes node-wide (caps a
//!   Sybil fan-out).
//! - **per-source** — concurrent in-flight from one `remote_id` (caps a
//!   single-source flood; by limiting any one source's share of the global
//!   pool it preserves ZEB-853 B7's anti-lockout property — no single source
//!   can monopolize the node-wide ceiling).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Per-source concurrent in-flight cap: how many handshakes a single
/// connection `remote_id` may have in flight at once. Well above honest use
/// (1-3 concurrent), it tightly bounds a single-source stalled flood and, by
/// capping any one source's share of the global pool, preserves the
/// anti-lockout property (ZEB-853 B7).
pub const HANDSHAKE_INFLIGHT_PER_SOURCE_MAX: usize = 8;

/// Node-wide concurrent in-flight cap: total handshakes in flight across all
/// sources. Sized ~10x a heavy legitimate join surge (honest handshakes hold a
/// slot < 1 s), so it never sheds honest load, yet caps a Sybil fan-out whose
/// uncapped worst case is (attacker connection rate) x 30 s `io_deadline`.
pub const HANDSHAKE_INFLIGHT_GLOBAL_MAX: usize = 1024;

/// A two-tier concurrency limiter over connection `remote_id`s. Shared
/// node-wide as an `Arc<InFlightHandshakeGate>`.
pub(crate) struct InFlightHandshakeGate {
    inner: Mutex<InFlightState>,
    per_source_max: usize,
    global_max: usize,
}

struct InFlightState {
    /// Total in-flight handshakes across all sources.
    global: usize,
    /// In-flight count per source. Self-bounding: every entry also counts
    /// toward `global`, and entries are removed at zero, so
    /// `per_source.len() <= global <= global_max`. No separate key cap needed
    /// (contrast `friend_intro::KeyedSlidingWindow`, whose rate entries linger
    /// after a request completes and therefore need `MAX_WINDOW_KEYS`).
    per_source: HashMap<[u8; 32], usize>,
}

impl InFlightHandshakeGate {
    /// The only constructor — both caps explicit. Production passes the
    /// constants (see `MultiplexHandshakeDispatcher::new`); tests pass tiny
    /// caps. Exposing only this (no no-arg `new`) avoids
    /// `clippy::new_without_default`.
    pub(crate) fn with_caps(per_source_max: usize, global_max: usize) -> Self {
        Self {
            inner: Mutex::new(InFlightState {
                global: 0,
                per_source: HashMap::new(),
            }),
            per_source_max,
            global_max,
        }
    }

    /// Reserve one in-flight slot for `source`. Returns an [`InFlightGuard`]
    /// that releases the slot on drop, or `None` if the global OR the
    /// per-source ceiling is already reached (→ caller sheds the connection).
    ///
    /// Checks the global ceiling first (Sybil fan-out), then the per-source
    /// ceiling (single-source flood). A zero cap admits nothing.
    pub(crate) fn try_acquire(self: &Arc<Self>, source: [u8; 32]) -> Option<InFlightGuard> {
        let mut st = self.inner.lock().unwrap();
        if st.global >= self.global_max {
            return None;
        }
        if st.per_source.get(&source).copied().unwrap_or(0) >= self.per_source_max {
            return None;
        }
        st.global += 1;
        *st.per_source.entry(source).or_insert(0) += 1;
        Some(InFlightGuard {
            gate: Arc::clone(self),
            source,
        })
    }

    /// Test-only snapshot of `(global count, distinct in-flight sources)`.
    #[cfg(test)]
    fn counts(&self) -> (usize, usize) {
        let st = self.inner.lock().unwrap();
        (st.global, st.per_source.len())
    }
}

/// RAII slot reservation. Dropping it releases one global + one per-source
/// slot for its `source`, removing the per-source entry when it reaches zero
/// (keeping the map bounded to sources with a live in-flight handshake).
pub(crate) struct InFlightGuard {
    gate: Arc<InFlightHandshakeGate>,
    source: [u8; 32],
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        // std::sync::Mutex (not tokio): Drop cannot be async, and this
        // critical section is O(1) with no await — a sync lock is correct and
        // necessary, and is never held across an await (no async stall).
        let mut st = self.gate.inner.lock().unwrap();
        st.global = st.global.saturating_sub(1);
        if let Some(c) = st.per_source.get_mut(&self.source) {
            *c -= 1;
            if *c == 0 {
                st.per_source.remove(&self.source);
            }
        }
    }
}
```

Add the module declaration in `src-tauri/src/lib.rs` (after line 197):

```rust
pub mod inflight_handshake_gate;
```

- [ ] **Step 2: Write the failing tests** (append to `inflight_handshake_gate.rs`).

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Distinct 32-byte source key from a small tag.
    fn src(n: u8) -> [u8; 32] {
        let mut s = [0u8; 32];
        s[0] = n;
        s
    }

    #[test]
    fn per_source_cap_sheds_beyond_max_and_recovers_on_drop() {
        let gate = Arc::new(InFlightHandshakeGate::with_caps(2, 100));
        let a = src(1);
        let g1 = gate.try_acquire(a).expect("1st admits");
        let _g2 = gate.try_acquire(a).expect("2nd admits (at per-source cap)");
        assert!(gate.try_acquire(a).is_none(), "3rd from same source is shed");
        drop(g1);
        assert!(
            gate.try_acquire(a).is_some(),
            "a slot frees for the same source once a guard drops"
        );
    }

    #[test]
    fn global_cap_sheds_new_source_under_its_per_source_cap() {
        // Per-source cap high (8) so only the GLOBAL cap (3) can bite.
        let gate = Arc::new(InFlightHandshakeGate::with_caps(8, 3));
        let _g1 = gate.try_acquire(src(1)).expect("global 1/3");
        let _g2 = gate.try_acquire(src(2)).expect("global 2/3");
        let _g3 = gate.try_acquire(src(3)).expect("global 3/3");
        // src(4) is brand-new (0 in-flight, under its per-source cap) yet the
        // node-wide ceiling is full → shed. This is the Sybil-fan-out bound.
        assert!(
            gate.try_acquire(src(4)).is_none(),
            "4th distinct source shed by the global ceiling despite being under per-source cap"
        );
    }

    #[test]
    fn dropping_all_guards_zeroes_global_and_empties_map() {
        let gate = Arc::new(InFlightHandshakeGate::with_caps(8, 100));
        let guards: Vec<_> = (0..5).map(|n| gate.try_acquire(src(n)).unwrap()).collect();
        let extra = gate.try_acquire(src(0)).unwrap(); // stack a 2nd on src(0)
        assert_eq!(gate.counts(), (6, 5), "6 in flight across 5 distinct sources");
        drop(extra);
        drop(guards);
        assert_eq!(
            gate.counts(),
            (0, 0),
            "all slots released; the per-source map self-bounds back to empty"
        );
    }

    #[test]
    fn one_source_at_cap_does_not_lock_out_others() {
        // Anti-lockout (ZEB-853 B7): source A saturating its per-source budget
        // must not prevent source B from acquiring.
        let gate = Arc::new(InFlightHandshakeGate::with_caps(2, 100));
        let a = src(1);
        let _a1 = gate.try_acquire(a).expect("A 1/2");
        let _a2 = gate.try_acquire(a).expect("A 2/2 (at per-source cap)");
        assert!(gate.try_acquire(a).is_none(), "A is shed beyond its cap");
        assert!(
            gate.try_acquire(src(2)).is_some(),
            "B still admits — one source does not lock out others"
        );
    }

    #[test]
    fn zero_caps_admit_nothing() {
        let g_global0 = Arc::new(InFlightHandshakeGate::with_caps(8, 0));
        assert!(
            g_global0.try_acquire(src(1)).is_none(),
            "global cap 0 admits nothing"
        );
        let g_source0 = Arc::new(InFlightHandshakeGate::with_caps(0, 100));
        assert!(
            g_source0.try_acquire(src(1)).is_none(),
            "per-source cap 0 admits nothing"
        );
    }
}
```

- [ ] **Step 3: Run the tests to verify they pass.**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(inflight_handshake_gate)'`
Expected: 5 tests pass.

> **Note (transient dead-code):** the primitive's only non-test consumer arrives in Task 2, so a `clippy --all-targets -D warnings` run *at this commit* would flag `InFlightHandshakeGate`/`try_acquire`/`with_caps` as unused in the lib target. That is expected and transient — do **not** add `#[allow(dead_code)]` churn. The nextest command above compiles the test build (where they ARE used) and is the Task-1 gate; the authoritative `-D warnings` clippy runs after Task 2 (ZEB-865 precedent — CI only ever sees the pushed HEAD, post-Task 2).

- [ ] **Step 4: Format and commit.**

```bash
cd src-tauri && cargo fmt --all
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/inflight_handshake_gate.rs src-tauri/src/lib.rs
git commit -m "ZEB-866: add InFlightHandshakeGate concurrency primitive"
```

---

### Task 2: Wire the gate into `MultiplexHandshakeDispatcher`

**Files:**
- Modify: `src-tauri/src/iroh_friend_acceptor.rs` — struct field (`:2533`), `new()`/`with_gate_caps` (`:2544`), `handle_connection` (`:2577`), + a construction test in the `tests` mod (near `:4351`).

**Interfaces:**
- Consumes (from Task 1): `crate::inflight_handshake_gate::{InFlightHandshakeGate, HANDSHAKE_INFLIGHT_PER_SOURCE_MAX, HANDSHAKE_INFLIGHT_GLOBAL_MAX}`; `InFlightHandshakeGate::with_caps`; `gate.try_acquire(source) -> Option<InFlightGuard>`.
- Produces: `MultiplexHandshakeDispatcher::with_gate_caps(invite, friend, pex, per_source_max, global_max)` (test/tuning seam); `new()` unchanged in signature.

- [ ] **Step 1: Add the import** near the top `use crate::...` block of `iroh_friend_acceptor.rs`:

```rust
use crate::inflight_handshake_gate::{
    InFlightHandshakeGate, HANDSHAKE_INFLIGHT_GLOBAL_MAX, HANDSHAKE_INFLIGHT_PER_SOURCE_MAX,
};
```

- [ ] **Step 2: Add the `gate` field** to `struct MultiplexHandshakeDispatcher` (`:2533`):

```rust
pub struct MultiplexHandshakeDispatcher {
    /// Receives `harmony/handshake/v1` connections (community-invite redemption).
    invite: Arc<dyn crate::iroh_invite_acceptor::IrohHandshakeDispatcher>,
    /// Receives `harmony/friend/v1` connections (friend-link handshake).
    friend: Arc<dyn crate::iroh_invite_acceptor::IrohHandshakeDispatcher>,
    /// ZEB-375: receives `harmony/friend-pex/v1` connections (referral catalog).
    pex: Arc<dyn crate::iroh_invite_acceptor::IrohHandshakeDispatcher>,
    /// ZEB-866: per-source + node-wide concurrency gate consulted before
    /// delegating, bounding in-flight handshakes against a pre-`accept_bi`
    /// slowloris. Shared node-wide.
    gate: Arc<InFlightHandshakeGate>,
}
```

- [ ] **Step 3: Update `new()` and add `with_gate_caps`** (replace the existing `new` at `:2544`):

```rust
impl MultiplexHandshakeDispatcher {
    /// Build a multiplexer over the invite + friend + friend-PEX acceptors,
    /// with the production in-flight gate caps.
    pub fn new(
        invite: Arc<dyn crate::iroh_invite_acceptor::IrohHandshakeDispatcher>,
        friend: Arc<dyn crate::iroh_invite_acceptor::IrohHandshakeDispatcher>,
        pex: Arc<dyn crate::iroh_invite_acceptor::IrohHandshakeDispatcher>,
    ) -> Self {
        Self::with_gate_caps(
            invite,
            friend,
            pex,
            HANDSHAKE_INFLIGHT_PER_SOURCE_MAX,
            HANDSHAKE_INFLIGHT_GLOBAL_MAX,
        )
    }

    /// Test/tuning constructor — same as `new` but with explicit in-flight gate
    /// caps so tests can drive the concurrency ceiling deterministically with
    /// tiny values.
    pub fn with_gate_caps(
        invite: Arc<dyn crate::iroh_invite_acceptor::IrohHandshakeDispatcher>,
        friend: Arc<dyn crate::iroh_invite_acceptor::IrohHandshakeDispatcher>,
        pex: Arc<dyn crate::iroh_invite_acceptor::IrohHandshakeDispatcher>,
        per_source_max: usize,
        global_max: usize,
    ) -> Self {
        Self {
            invite,
            friend,
            pex,
            gate: Arc::new(InFlightHandshakeGate::with_caps(per_source_max, global_max)),
        }
    }

    /// Test-only accessor for the installed gate (proves the wiring caps).
    #[cfg(test)]
    pub(crate) fn gate_for_test(&self) -> &Arc<InFlightHandshakeGate> {
        &self.gate
    }
}
```

- [ ] **Step 4: Wire `handle_connection`** (replace the impl body at `:2577`):

```rust
#[async_trait]
impl crate::iroh_invite_acceptor::IrohHandshakeDispatcher for MultiplexHandshakeDispatcher {
    async fn handle_connection(&self, conn: Connection) {
        // ZEB-866: bound concurrent in-flight handshakes BEFORE delegating to
        // the inner acceptor (whose first await is `accept_bi`, up to
        // `io_deadline`). `remote_id()` is available pre-`accept_bi` (ZEB-616).
        // The permit is held across the whole handshake and released when
        // `_guard` drops; shedding here — before `accept_bi` — is what stops a
        // stalled / withheld-stream flood from pinning handler tasks.
        let source = *conn.remote_id().as_bytes();
        let _guard = match self.gate.try_acquire(source) {
            Some(g) => g,
            None => {
                tracing::warn!(
                    remote_id = ?conn.remote_id(),
                    "ZEB-866: in-flight handshake gate shed (per-source or global cap reached)"
                );
                // Clean CONNECTION_CLOSE, zero response-stream bytes — the same
                // benign, retryable shape as the Tier-1 post-`accept_bi` shed
                // (mirrors the boot-queue close at zenoh_iroh_transport.rs:544).
                conn.close(0u32.into(), b"zeb866-inflight-cap");
                return;
            }
        };
        // Re-read the negotiated ALPN the accept loop already matched and
        // delegate the owned connection to the selected acceptor.
        self.select_for_alpn(conn.alpn())
            .handle_connection(conn)
            .await;
    }
}
```

- [ ] **Step 5: Add the construction test** in the `tests` mod (after `multiplexer_selects_friend_stub_for_friend_alpn_and_invite_stub_otherwise`, near `:4383`):

```rust
    #[test]
    fn multiplex_dispatcher_gate_enforces_configured_caps() {
        // The dispatcher's `handle_connection` gate wiring can't be unit-driven
        // (no in-process `Connection`), so prove the seam a different way: that
        // `with_gate_caps` installs a live gate enforcing exactly those caps.
        let stub = || -> Arc<dyn IrohHandshakeDispatcher> {
            Arc::new(RecordingDispatcher {
                called: AtomicBool::new(false),
            })
        };
        let mux = MultiplexHandshakeDispatcher::with_gate_caps(stub(), stub(), stub(), 2, 100);
        let gate = mux.gate_for_test();
        let mut key = [0u8; 32];
        key[0] = 7;
        let _g1 = gate.try_acquire(key).expect("1st admits");
        let _g2 = gate.try_acquire(key).expect("2nd admits (at per-source cap)");
        assert!(
            gate.try_acquire(key).is_none(),
            "the dispatcher's gate enforces the per-source cap it was constructed with"
        );
    }
```

- [ ] **Step 6: Run the scoped tests to verify they pass.**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(multiplex) + test(route_handshake) + test(inflight_handshake_gate)'`
Expected: the new `multiplex_dispatcher_gate_enforces_configured_caps` + existing routing tests + Task 1's gate tests all pass.

- [ ] **Step 7: Full CI-parity gate (authoritative — the gate is now wired, no dead-code).**

Run each, reading each command's own exit (no pipes):
```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
Expected: fmt clean; clippy clean (`-D warnings`); full suite green (baseline 5626 + this branch's new tests; 0 failures).

- [ ] **Step 8: Commit.**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/iroh_friend_acceptor.rs
git commit -m "ZEB-866: gate in-flight handshakes at the multiplex chokepoint"
```

---

## Self-Review

**1. Spec coverage:**
- Concurrency primitive (per-source + global, RAII release, self-bounding map) → Task 1. ✓
- `std::sync::Mutex` Drop-safety rationale → Task 1 code comment. ✓
- Chokepoint wiring (read remote_id → try_acquire → guard-or-shed → delegate), covering both entry paths uniformly → Task 2 Step 4. ✓
- Shed = clean CONNECTION_CLOSE, zero bytes → Task 2 Step 4 (`conn.close(0u32.into(), b"zeb866-inflight-cap")`). ✓
- Constructor seam (`new` constants + `with_gate_caps` test seam, no call-site change) → Task 2 Steps 3. ✓
- Anti-lockout preserved (per-source caps any one source's global share) → Task 1 test `one_source_at_cap_does_not_lock_out_others`. ✓
- Ticket's explicit "single-source stalled flood shed before occupying slots" → satisfied by Task 1 tests `per_source_cap_sheds_beyond_max_and_recovers_on_drop` + `one_source_at_cap_does_not_lock_out_others` (per-source refusal before delegation + no cross-source lockout). ✓
- Loopback wiring test deferred to ZEB-870 → out of scope here; ZEB-870 description to be expanded during convergence/PR. ✓

**2. Placeholder scan:** No TBD/TODO; all code blocks complete; every referenced symbol (`InFlightHandshakeGate`, `try_acquire`, `with_gate_caps`, `gate_for_test`, constants) is defined in Task 1 or Task 2. ✓

**3. Type consistency:** `try_acquire(source: [u8; 32]) -> Option<InFlightGuard>`; `with_caps(per_source_max, global_max)`; `with_gate_caps(invite, friend, pex, per_source_max, global_max)`; caps `usize`; source key `[u8; 32]` (matches `*conn.remote_id().as_bytes()`). Consistent across both tasks. ✓
