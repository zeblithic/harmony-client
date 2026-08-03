# ZEB-861 — Divergence-safe derived-state bounds for Tier-3 voting ingest — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bound the Tier-3 community-voting ingest's derived state (lane-map scan cost + materialized projection) divergence-safely, without altering the ZEB-860 rebuild trigger or the ZEB-850/ZEB-320 watermark invariants.

**Architecture:** Three independent, divergence-safe bounds in two files — a decode-time length predicate + a per-actor materialization cap in the kernel + an O(1) rewrite of a hot-path fold. Each is either a uniform predicate on the event or a pure function of canonically-ordered state, so all replicas converge (the ZEB-860 property). Spec: `docs/superpowers/specs/2026-08-03-zeb-861-voting-ingest-derived-state-bounds.md`.

**Tech Stack:** Rust (Tauri backend, `src-tauri/`); `cargo nextest`; kernel = `community_voting_tier3.rs`, ingest engine = `community_voting_log_engine.rs`.

## Global Constraints

- MSRV 1.91. CI gates (run from `src-tauri/`): `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.
- All new constants are `pub(crate) const` with the exact values below: `MAX_DEVICE_ID_LEN = 64`, `MAX_DECLINES_PER_ACTOR = 2`, `MAX_DRAFT_CANDIDATES_PER_ACTOR = 5`, `MAX_RATIFICATION_BALLOTS_PER_ACTOR = 2`.
- **No change** to the ZEB-860 rebuild trigger set (`{ss,md,ds,dv}`), to `last_received_hlc`'s advance-on-every-dispatch invariant (ZEB-850), or to `last_hlc`'s advance-on-accept-only invariant (ZEB-320).
- The Component-1 length guard MUST appear in BOTH ingest routes (`process_inbound` and `apply_backfilled_event`) — parity requirement.
- Line numbers below are as of 2026-08-03 recon and may have drifted; locate by symbol name.
- Iterative gates may use `scripts/test-select --context task`; the FINAL pre-PR sweep is the full `--workspace --all-targets` command above.

---

### Task 1: Component 1 — `device_id` length cap at both decode routes

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs` — `process_inbound` (~`:2890`, after `ciborium::from_reader`, before the forward-skew block ~`:2906`); `apply_backfilled_event` (~`:3035`, after decode, before skew ~`:3051`); add the module const.
- Test: same file's `#[cfg(test)]` module (mirror the existing `process_inbound_rejects_far_future` skew-reject test at ~`:5748`).

**Interfaces:**
- Produces: `pub(crate) const MAX_DEVICE_ID_LEN: usize = 64;` (no downstream consumer in other tasks).

- [ ] **Step 1: Write the failing tests.** Mirror the sibling skew-reject test's harness. Two tests (one per route):

```rust
#[tokio::test]
async fn process_inbound_rejects_over_length_device_id() {
    // Build a validly-shaped SignedVotingEvent whose hlc.device_id is
    // MAX_DEVICE_ID_LEN + 1 chars (e.g. "a".repeat(MAX_DEVICE_ID_LEN + 1)),
    // cbor-encode it, and feed the bytes to process_inbound exactly as the
    // skew-reject test does. Assert Err whose message contains
    // "device_id length" and "exceeds".
}

#[tokio::test]
async fn apply_backfilled_rejects_over_length_device_id() {
    // Same event bytes → apply_backfilled_event → assert the same Err.
}
```

- [ ] **Step 2: Run the tests to verify they fail.**
Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(over_length_device_id)'`
Expected: FAIL (events currently accepted past decode → no such Err).

- [ ] **Step 3: Add the const + the guard in both routes.**

```rust
/// Max byte-length of an accepted voting-event `hlc.device_id`. Canonical
/// ids are 32-hex (16-byte identity hash); engine-auto lanes are shorter.
/// 64 = 2x margin + 256-bit-hash headroom. Rejects decode-time key bloat.
pub(crate) const MAX_DEVICE_ID_LEN: usize = 64;
```

In each route, immediately after the `let event: SignedVotingEvent = ciborium::from_reader(...)?;` line and before the skew block:

```rust
if event.hlc.device_id.len() > MAX_DEVICE_ID_LEN {
    return Err(format!(
        "voting event device_id length {} exceeds MAX_DEVICE_ID_LEN {}",
        event.hlc.device_id.len(),
        MAX_DEVICE_ID_LEN
    ));
}
```

- [ ] **Step 4: Run the tests to verify they pass.**
Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(over_length_device_id)'`
Expected: PASS.

- [ ] **Step 5: Verify a canonical (32-char) and boundary (exactly 64) id still pass.** Add an assertion (or a third test `process_inbound_accepts_max_length_device_id`) that a 32-char hex id and a 64-char id are NOT rejected by the length guard (they may fail later gates, but not with the length error). Run the same filter; expected PASS.

- [ ] **Step 6: Gate + commit.**
Run: `cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` then `scripts/test-select --context task`.
```bash
git add src-tauri/src/community_voting_log_engine.rs
git commit -m "ZEB-861 T1: device_id length cap (64B) at both voting ingest routes"
```

---

### Task 2: Component 2′ — O(1) `max_received_hlc()` via `max_applied`

**Files:**
- Modify: `src-tauri/src/community_voting_tier3.rs` — `max_received_hlc` (~`:1168-1178`).
- Test: same file's `#[cfg(test)]` module (near the existing `max_received_hlc_is_max_over_lanes` at ~`:5176`).

**Interfaces:**
- Produces: behavior-preserving `max_received_hlc` (identical `(wall_ms, logical)` output; empty `device_id`). No new interface.

- [ ] **Step 1: Write the failing equivalence test.** Assert the new impl equals the old fold across a spread of dispatch sequences (multi-lane, accepts, silent-drops, out-of-order, empty). Write it against the OLD fold expression inline so it fails until the impl is swapped:

```rust
#[test]
fn max_received_hlc_equals_max_applied_prefix() {
    // Build a Tier3PollState, apply_event a mix of accepted + silently-dropped
    // events across several (actor, device_id) lanes and out-of-order HLCs
    // (mirror the setup in max_received_hlc_is_max_over_lanes).
    // Expected = the old fold, computed directly here:
    let expected = poll.last_received_hlc.values().copied().max()
        .map(|(w, l)| (w, l));
    let got = poll.max_received_hlc().map(|h| (h.wall_ms, h.logical));
    assert_eq!(got, expected, "O(1) max_received_hlc must equal max-over-lanes");
    // Also assert None on a fresh poll (no dispatch).
}
```

- [ ] **Step 2: Run it — expect it to still PASS against the current fold** (the current impl already equals the fold; this test pins the invariant BEFORE the swap so the swap is proven behavior-preserving).
Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(max_received_hlc)'`
Expected: PASS (both existing `max_received_hlc_is_max_over_lanes` and the new test).

- [ ] **Step 3: Swap the impl to O(1).**

```rust
pub fn max_received_hlc(&self) -> Option<Hlc> {
    // O(1): (wall_ms, logical) prefix of max_applied is identical to the max
    // over all per-lane (wall_ms, logical) — both advance on every dispatch.
    // device_id is documented-unused by the sole consumer (kd=rs mint floor).
    self.max_applied.as_ref().map(|(wall_ms, logical, _device)| Hlc {
        wall_ms: *wall_ms,
        logical: *logical,
        device_id: String::new(),
    })
}
```

- [ ] **Step 4: Run the tests to verify both still pass.**
Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(max_received_hlc)'`
Expected: PASS (the equivalence test now exercises the O(1) path against the fold expression).

- [ ] **Step 5: Gate + commit.**
Run: fmt + clippy (`--all-targets`) as in Task 1, then `scripts/test-select --context task`.
```bash
git add src-tauri/src/community_voting_tier3.rs
git commit -m "ZEB-861 T2: O(1) max_received_hlc via max_applied (drop per-lane fold)"
```

---

### Task 3: Component 3 — per-actor materialization caps (md / dc / rb)

**Files:**
- Modify: `src-tauri/src/community_voting_tier3.rs` — `Tier3PollState` struct (add 3 counter fields near `declines`/`candidates`/`ratification_ballots`, ~`:198-202`); `new_from_create` (reset the 3 fields, ~`:451-473`); the `md` arm (~`:570-575`), `dc` arm (~`:757-769`), `rb` arm (~`:912`/`:915`); the inaccurate `rb` comment (~`:2024-2030`); add 3 consts (near the `ds` 5-cap at ~`:618`).
- Test: same file's `#[cfg(test)]` module (mirror the existing `ds` 5-cap test).

**Interfaces:**
- Consumes: nothing from Tasks 1–2.
- Produces: `pub(crate) const MAX_DECLINES_PER_ACTOR: u8 = 2;`, `MAX_DRAFT_CANDIDATES_PER_ACTOR: u8 = 5;`, `MAX_RATIFICATION_BALLOTS_PER_ACTOR: u8 = 2;`; fields `declines_per_actor`, `candidates_per_actor`, `ballots_per_actor: BTreeMap<OwnerAddr, u8>`.

- [ ] **Step 1: Write the failing per-cap tests.** Three tests, each mirroring the `ds` 5-cap test's event construction (same module). For each kind, submit `LIMIT + 1` events from one actor and assert exactly `LIMIT` materialize:

```rust
#[test]
fn md_cap_limits_declines_per_actor() {
    // Apply MAX_DECLINES_PER_ACTOR + 1 kd=md events from actor A (distinct HLCs
    // so none is monotonic-dropped). Assert poll.declines.iter()
    //   .filter(|(a, _)| *a == A).count() == MAX_DECLINES_PER_ACTOR as usize.
    // Assert the excess apply returned Ok(Dropped) (advance_last_hlc == false).
    // Assert a second actor B's decline still materializes (per-actor, not global).
}

#[test]
fn dc_cap_limits_candidates_per_actor() {
    // Same shape against kd=dc → poll.candidates filtered by proposer == Some(A).
    // Limit = MAX_DRAFT_CANDIDATES_PER_ACTOR.
}

#[test]
fn rb_cap_limits_ballots_per_actor() {
    // Same shape against kd=rb (drive the poll to Ratification as the existing
    // rb tests do). Limit = MAX_RATIFICATION_BALLOTS_PER_ACTOR. Since ballots
    // carry no actor tag, assert poll.ratification_ballots.len() bounded and
    // poll.ballots_per_actor.get(&A) == Some(&MAX_RATIFICATION_BALLOTS_PER_ACTOR).
}
```

- [ ] **Step 2: Run to verify failure.**
Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(_cap_limits_)'`
Expected: FAIL (no caps → all `LIMIT + 1` materialize; fields don't exist yet → compile error is acceptable "fail").

- [ ] **Step 3: Add the 3 consts + 3 counter fields + reset.**

Consts (near the `ds` cap):
```rust
pub(crate) const MAX_DECLINES_PER_ACTOR: u8 = 2;          // md: a member declines once (+1 resubmit margin)
pub(crate) const MAX_DRAFT_CANDIDATES_PER_ACTOR: u8 = 5;  // dc: matches the ds cap; ≤4 non-sq advance anyway
pub(crate) const MAX_RATIFICATION_BALLOTS_PER_ACTOR: u8 = 2; // rb: one ballot per member (+1 LWW margin)
```

Fields on `Tier3PollState` (siblings of the guarded Vecs):
```rust
/// ZEB-861 per-actor materialization caps (replay-derived; reset on rebuild).
pub declines_per_actor: std::collections::BTreeMap<OwnerAddr, u8>,
pub candidates_per_actor: std::collections::BTreeMap<OwnerAddr, u8>,
pub ballots_per_actor: std::collections::BTreeMap<OwnerAddr, u8>,
```

In `new_from_create`, initialize all three to `std::collections::BTreeMap::new()`.

- [ ] **Step 4: Add the cap gate to each arm** (mirror the `ds` 5-cap `:609-650` — drop without pushing when at limit, increment only on a real push).

`md` arm:
```rust
let prior = self.declines_per_actor.get(&ev.actor).copied().unwrap_or(0);
if prior >= MAX_DECLINES_PER_ACTOR {
    advance_last_hlc = false;
    tracing::debug!(actor = %ev.actor, "kd=md drop: per-actor decline cap reached");
} else {
    self.declines.push((ev.actor, ev.hlc.clone()));
    *self.declines_per_actor.entry(ev.actor).or_insert(0) += 1;
}
```
`dc` arm: identical shape against `candidates` / `candidates_per_actor` / `MAX_DRAFT_CANDIDATES_PER_ACTOR` (wrap the existing `candidates.push(DraftCandidateState{..})`).
`rb` arm: identical shape against `ratification_ballots` / `ballots_per_actor` / `MAX_RATIFICATION_BALLOTS_PER_ACTOR`, keyed on `ev.actor`; guard BOTH push sites (se `:912`, pu `:915`) — count once per event, before the mode branch, so a single ballot doesn't double-count.

- [ ] **Step 5: Fix the inaccurate `rb` comment** (~`:2024-2030`): replace the "apply path already enforces 1-per-actor via current_mini_public + monotonic-HLC" claim with: the kernel now enforces `≤ MAX_RATIFICATION_BALLOTS_PER_ACTOR` per actor via `ballots_per_actor`.

- [ ] **Step 6: Run the per-cap tests to verify they pass.**
Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(_cap_limits_)'`
Expected: PASS.

- [ ] **Step 7: Write + run the divergence-safety convergence test.** Mirror the ZEB-860 `reconcile_converges_*` pattern:

```rust
#[test]
fn per_actor_caps_converge_across_delivery_orders() {
    // Two Tier3PollStates. Feed an over-cap md (and/or dc) sequence from one
    // actor in two DIFFERENT delivery orders, then force a canonical rebuild
    // on each (rebuild_from_events). Assert the two materialized sets
    // (declines / candidates / *_per_actor) are byte-for-byte identical.
}
```
Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(per_actor_caps_converge)'`
Expected: PASS (the caps are a pure function of the canonical-ordered set).

- [ ] **Step 8: Gate + commit.**
Run: `cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, then `scripts/test-select --context task`.
```bash
git add src-tauri/src/community_voting_tier3.rs
git commit -m "ZEB-861 T3: per-actor materialization caps for md/dc/rb (mirror ds 5-cap)"
```

---

## Final gate (after all tasks + review)

- [ ] Full CI-parity sweep from `src-tauri/`: `cargo fmt --all -- --check` && `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` && `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. All green before PR.

## Self-review (author checklist — completed at plan-write time)

- **Spec coverage:** Component 1 → Task 1; Component 2′ → Task 2; Component 3 (md/dc/rb caps + fields + reset + comment fix) → Task 3; spec §8 tests 1–4 → Tasks 1/2/3; test 5 (full sweep) → Final gate. Residual (§6) and ZEB-868 re-scope (§7) are non-code (handled in PR body + Linear). Covered.
- **Placeholder scan:** all steps carry concrete code or exact commands; test bodies give assertions + point to the sibling test for event-builders (the builders exist in the same module).
- **Type consistency:** const names/values and field names/types match the spec §3/§5 and the Interfaces blocks verbatim; `u8` counters mirror `statements_per_author`.
