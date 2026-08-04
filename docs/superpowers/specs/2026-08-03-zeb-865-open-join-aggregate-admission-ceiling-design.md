# ZEB-865: Node-wide aggregate open-join admission ceiling — Design

**Ticket:** ZEB-865 (ZEB-853 B7 follow-up) — Sybil-flood defense-in-depth.
**Status:** Design approved (Approach A; ceiling = 1024/60 s).

## Problem

The open-join admission path (`open_join_admit::verify_and_admit_open_join`) is
the security core that admits a tokenless joiner to a public community. ZEB-853
B7 re-keyed its admission budget (`OpenJoinRateLimiter`) from a single global
20/60 s counter to **per-source** (keyed on the connecting, un-spoofable
`remote_id`), so one flooding source can no longer exhaust the shared budget and
lock out every legitimate open-joiner (the pre-B7 global-lockout DoS).

The deliberate residual: with **no** node-wide aggregate ceiling, a Sybil
attacker holding the public open-invite link can mint many endpoint identities,
each receiving an independent per-source budget, and multiply the expensive
post-auth work — membership materialization + the `O(events)` log clone +
`bootstrap_admit_open_publisher` — across the fan-out. The theoretical
uncapped aggregate is `MAX_WINDOW_KEYS × OPEN_JOIN_RATE_LIMIT_PER_WINDOW =
8192 × 20 = 163,840` admissions / 60 s.

Two per-source shields already bound the *per-source* cost: the pre-decode
Tier-1 connection shield (`OpenJoinConnLimiter`, 40/1 h) bounds per-source
connections and pre-consent crypto; the per-source admission budget (20/60 s)
bounds per-source materialization. This ticket adds the missing **aggregate**
bound.

## Approach (A): aggregate ceiling at the admission layer, after crypto

Add a node-wide sliding-window counter inside `OpenJoinRateLimiter`, checked
inside `verify_and_admit_open_join` at step 7 (the existing replay + rate-limit
stage), immediately **before** step 8–9 (ban-check materialization + admission).

Placement rationale — the ceiling is reached **only by fully-verified
requests**: a request has already passed the capability MAC (step 3), the
device-hash binding (step 4), the enrollment cert (step 5), and the ed25519
packet-signature `verify_strict` (step 6) before it can pressure the ceiling.
This is what makes the ceiling safe:

- It sheds the **dominant, community-size-scaling** cost (materialization +
  `O(events)` clone), matching the ticket's "tier it below the expensive
  materialization step."
- It **cannot be cheaply exhausted**. Placing the ceiling *before* signature
  verification (rejected Approach B) would let anyone holding the public link
  mint a valid capability MAC cheaply and exhaust the ceiling with
  (valid-cap, garbage-sig) requests — reintroducing the exact global-lockout
  the ticket warns against. Requiring a valid enrollment + ed25519 signature to
  reach the ceiling means an attacker pays real per-request crypto, and each
  such request is already per-source-bounded by the conn shield.
- The 2× ed25519 verify on a shed request is already per-source-bounded by the
  Tier-1 conn shield (40/1 h); bounding *aggregate* crypto would require a
  global gate at the pre-decode conn layer, which is shared with the
  invite-redeem (0x10) flow and is the separate ZEB-866 (pre-`accept_bi`)
  concern. **Out of scope here.**

Rejected alternatives: **B** (ceiling before crypto — cheap-exhaustion lockout);
**C** (global cap at the conn shield — throttles token-bearing invites, and is
ZEB-866's pre-decode territory).

## Mechanism

### The global window

`OpenJoinRateLimiter` gains one field:

```rust
/// ZEB-865: node-wide aggregate admission ceiling, checked in ADDITION to the
/// per-source `windows`. A single unit-key reuse of the audited sliding-window
/// primitive: it is exactly one global 60 s window (the MAX_WINDOW_KEYS
/// eviction is a no-op at one key). Bounds total expensive admission work
/// against a Sybil fan-out that the per-source budget alone cannot (each fake
/// source gets its own per-source window; only this aggregate gate caps the sum).
global: KeyedSlidingWindow<()>,
```

Constant:

```rust
/// ZEB-865: node-wide aggregate admissions accepted per
/// `OPEN_JOIN_RATE_LIMIT_WINDOW_MS` before excess is shed as `NodeCapacity`.
/// 1024 = 51× the per-source budget: far above any realistic single-beacon
/// honest burst (joiners also spread across the butler set and retry on shed),
/// while cutting the uncapped Sybil worst case (~163,840/60 s) ~160×. This is a
/// defense-in-depth backstop atop the per-source admission budget and the
/// Tier-1 connection shield, so it is sized to favor never locking out honest
/// load; raise it if single-beacon join bursts are observed to approach it.
pub const OPEN_JOIN_GLOBAL_ADMIT_MAX: usize = 1024;
```

### Peek/record split on the primitive

`KeyedSlidingWindow` gains one pure, non-mutating peek so the global gate can be
*checked* before the per-source gate *records*, and *recorded* only after the
per-source gate admits:

```rust
/// `true` if `key` would be admitted right now WITHOUT recording — the
/// non-mutating companion to `admit` (same `key: K` by-value signature),
/// for composing two gates where the second must not leave a phantom record
/// if the first sheds. Counts only in-window (non-stale) entries; a zero cap
/// never admits.
pub(crate) fn would_admit(&self, key: K, now_ms: u64) -> bool {
    if self.max == 0 {
        return false;
    }
    let cutoff = now_ms.saturating_sub(self.window_ms);
    match self.windows.get(&key) {
        None => true,
        Some(dq) => dq.iter().filter(|&&t| t >= cutoff).count() < self.max,
    }
}
```

### Limiter methods

```rust
/// ZEB-865: node-wide aggregate capacity peek (no record). Composed BEFORE the
/// per-source `allow` so a ceiling shed charges neither the source's budget nor
/// its nonce.
fn global_has_capacity(&self, limiter_now_ms: u64) -> bool {
    self.global.would_admit((), limiter_now_ms)
}

/// ZEB-865: commit one aggregate token. Called ONLY after `allow` admits, so a
/// per-source shed never drains the global ceiling (which would let one spammer
/// re-create single-source lockout).
fn record_global(&mut self, limiter_now_ms: u64) {
    self.global.admit((), limiter_now_ms);
}
```

### Constructor shape

Mirror `OpenJoinConnLimiter`: `new()` uses production consts; `with_caps`
injects tiny caps for deterministic tests.

The window is **not** a `with_caps` parameter: both windows and the nonce-replay
horizon in `is_replay` (defined as 4× the window) share the single protocol
constant `OPEN_JOIN_RATE_LIMIT_WINDOW_MS`. Exposing `window_ms` would let a caller
desync replay retention from the admission window, so only the caps vary.

```rust
pub fn new() -> Self {
    Self::with_caps(OPEN_JOIN_RATE_LIMIT_PER_WINDOW, OPEN_JOIN_GLOBAL_ADMIT_MAX)
}

pub fn with_caps(per_source_max: usize, global_max: usize) -> Self {
    Self {
        windows: KeyedSlidingWindow::new(per_source_max, OPEN_JOIN_RATE_LIMIT_WINDOW_MS),
        global: KeyedSlidingWindow::new(global_max, OPEN_JOIN_RATE_LIMIT_WINDOW_MS),
        seen_nonces: HashSet::new(),
        nonce_seen_at: HashMap::new(),
        epoch: tokio::time::Instant::now(),
    }
}
```

### Reject variant

```rust
/// Node-wide aggregate admission ceiling exceeded (ZEB-865). Distinct from
/// `RateLimited` (per-source): the source is within its own budget but the node
/// is at aggregate capacity. Same benign typed-rejection wire behavior as the
/// other post-decode rejects.
NodeCapacity,
```

### Composite decision — `admit_source` (step 7)

The step-7 ordering is encapsulated in **one** limiter method so production and
tests share exactly one composition (no test-replicates-prod-ordering drift):

```rust
/// ZEB-865: the whole rate-limit decision for one open-join request —
/// replay + node-wide aggregate ceiling + per-source budget + nonce record, in
/// the one order that keeps a ceiling shed from charging the source's budget or
/// nonce, and keeps a per-source shed from draining the aggregate ceiling.
fn admit_source(
    &mut self,
    source: [u8; 32],
    nonce: &[u8; 16],
    limiter_now_ms: u64,
) -> Result<(), OpenJoinReject> {
    if self.is_replay(nonce, limiter_now_ms) {
        return Err(OpenJoinReject::Replay);        // 1. no record
    }
    if !self.global_has_capacity(limiter_now_ms) {
        return Err(OpenJoinReject::NodeCapacity);  // 2. PEEK — records nothing
    }
    if !self.allow(source, limiter_now_ms) {
        return Err(OpenJoinReject::RateLimited);   // 3. per-source check+record (UNCHANGED)
    }
    self.record_global(limiter_now_ms);            // 4. commit aggregate token
    self.record_nonce(nonce, limiter_now_ms);      // 5. commit nonce
    Ok(())
}
```

`verify_and_admit_open_join` step 7 collapses to
`limiter.admit_source(source_id, &req.nonce, limiter_now_ms)?;`, replacing the
current inline `is_replay` / `allow` / `record_nonce` trio. Behavior for every
existing path is byte-identical (same order, `allow` untouched, global cap
1024 ≫ per-source 20).

Correctness properties this ordering guarantees:

- **No phantom per-source charge on a node-wide shed.** A ceiling shed (step 2)
  returns before `allow` records, so the honest source keeps its full budget and
  its nonce (step 5 skipped) — the request is cleanly retryable, exactly like the
  existing `RateLimited` path.
- **No single-source drain of the ceiling.** `record_global` (step 4) fires only
  after `allow` admits, so a source spamming past its own 20/60 s (each shed at
  step 3) never consumes a global token — one spammer cannot exhaust the
  aggregate ceiling and re-create the pre-B7 lockout.
- **Existing behavior byte-identical.** `allow` is unchanged and the global cap
  (1024) is far above the per-source cap (20), so every existing single-source
  test peeks a non-full ceiling at step 2 and reaches step 3 exactly as before.
- **Race-free.** `verify_and_admit_open_join` holds `&mut OpenJoinRateLimiter`
  (the acceptor serializes calls under a `TokioMutex`), so the step-2 peek and
  step-4 record are a consistent read-modify with no interleaving — no TOCTOU.

## Components (isolation)

| Unit | Responsibility | Depends on |
|---|---|---|
| `KeyedSlidingWindow::would_admit` | Pure non-mutating capacity peek | nothing (self) |
| `OpenJoinRateLimiter.global` + methods | Node-wide aggregate rate state | `KeyedSlidingWindow` |
| `verify_and_admit_open_join` step 7 | Compose replay + per-source + aggregate + nonce | limiter methods |

The acceptor (`iroh_invite_acceptor.rs`) is **untouched**: the ceiling lives
entirely inside the `&mut` limiter, and `verify_and_admit_open_join`'s signature
is unchanged. A `NodeCapacity` reject flows through the existing
`write_open_join_rejection` typed-rejection path (it already stringifies the
reject via `format!("{reject:?}")`, so a new variant needs no acceptor change).

## Error handling / oracle-safety

Unlike the pre-decode conn shield (which sheds silently — zero bytes, the
ZEB-864 property — because the packet type isn't yet known), the admission layer
runs after decode and after the capability proof, and already answers every
reject (`Banned`, `Stale`, `RateLimited`, …) with a typed rejection so the
joiner UI can show a reason. `NodeCapacity` joins that set: same benign wire
behavior, distinct log/metric line. It is not a membership oracle — the joiner
has already proven capability to reach this layer.

## Testing

**`open_join_admit.rs` (direct limiter methods — cheap, no crypto):**

1. `global_ceiling_bounds_aggregate_across_sources` — `with_caps(20, 3)`:
   three *distinct* sources each admit once (fills the ceiling), a fourth
   distinct source that is well within its own per-source budget is shed
   `NodeCapacity`. Proves the aggregate bound and that it sheds under-budget
   sources.
2. `global_ceiling_does_not_relock_single_source` — high global cap: source A
   fills its own 20/60 s → `RateLimited` (not `NodeCapacity`); source B still
   admits. No regression of B7's anti-lockout property.
3. `single_source_shed_does_not_drain_global_ceiling` — the discriminating test:
   `with_caps(20, 30)`, source A makes 25 attempts (20 admit + 5 shed
   `RateLimited`). Correct behavior: A consumed exactly 20 global tokens (its
   admits, not its sheds), so exactly **10** further distinct sources admit and
   the 11th is `NodeCapacity`. A bug where per-source sheds drained the ceiling
   would leave only 5 (30 − 25) — so asserting exactly 10 is the discriminator.
4. `global_ceiling_keys_on_monotonic_clock` — fill the ceiling; advance only the
   limiter's monotonic clock past the window → admits resume (wall clock held
   fixed, mirroring the per-source monotonic test).
5. `globally_shed_request_nonce_is_retryable` — through `verify_and_admit_open_join`
   with `with_caps` (tiny global cap): a `NodeCapacity` shed does not persist the
   nonce; after the window rolls, the same nonce admits.

**`friend_intro.rs`:**

1. `keyed_sliding_window_would_admit_does_not_record` — `would_admit` returns the
   same result on repeated calls and never advances the count; a following
   `admit` still succeeds; a full window's `would_admit` returns false; a
   zero-cap window's `would_admit` returns false.

**Regression:** the full existing `open_join_admit` test module passes
unchanged (per-source paths are byte-identical).

## Scope boundaries (explicitly out)

- **Aggregate pre-consent crypto** (bounding the 2× ed25519 across N sources
  before decode) — requires a pre-`accept_bi` / conn-layer gate shared with the
  invite-redeem flow → **ZEB-866**.
- **Dispatcher-path coverage / any acceptor change** — none needed; the acceptor
  is untouched.
- **Tuning the ceiling per-community or dynamically** — a single node-wide const
  is the YAGNI choice; revisit only if observed load warrants.
