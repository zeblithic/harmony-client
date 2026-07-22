# ZEB-316 Deterministic HLC for engine-auto mints — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make engine-auto Tier-3 mints (kd=cl, kd=rs **pu-mode**, kd=sf) derive their HLC deterministically from replica-identical state so peer engines produce bit-identical events, then re-enable the orchestration hook on the inbound dispatch path. **se-mode kd=rs is excluded** (C1 refinement — stays on wall-clock `reserve_next_local_hlc`; its result converges via Lagrange invariance + LWW, and a deterministic-monotonic base is unachievable — see Task 3 (e)).

**Architecture:** A pure `engine_auto_hlc_from_base(base, pid, kind)` produces an HLC strictly-newer than `base` with a poll-derived device-id lane. Close/sortition-failed mints anchor to the *triggering event's* HLC (threaded from the caller); the **pu-mode** result mint anchors to a new `close_hlc` state field read from poll state. With identical signing key + actor + payload already guaranteed, an identical HLC makes the whole signed event byte-identical → trivial LWW. (se-mode kd=rs cannot use this: its committee kd=ts land after the close at per-replica walls above it, so a deterministic close-anchored kd=rs would be non-monotonic — it keeps wall-clock and relies on result-convergence + LWW.)

**Tech Stack:** Rust (tokio async), `ed25519_dalek`, the harmony-client voting engine (`community_voting_log_engine.rs`, `community_voting_tier3.rs`, `community_voting_core.rs`).

## Global Constraints

- All cargo commands run **from `src-tauri/`**. CI gates (must pass before PR): `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. Always include `--locked` and `--features test-fixtures`.
- **Relink cost:** a change to `src-tauri/src/**` (lib) relinks ~97 integration binaries (~50 min for a full nextest). During iterative per-task dev, scope test runs: `cargo nextest run --locked --features test-fixtures --lib` for lib unit tests, or `-E 'test(<name>)'` / `--test community_voting/...` for a single integration test. Run the **full** `--workspace --all-targets` sweep only as the final pre-PR gate.
- `Hlc` = `{ wall_ms: u64, logical: u32, device_id: String }` (`owner_state_types.rs:318`), ordered by the tuple `(wall_ms, logical, device_id)`. All three fields are in `signing_bytes`.
- Deterministic device-id lanes follow the D-FROST beacon precedent (`community_voting_log_engine.rs:745`): `poll_id_prefix = hex::encode(&poll_id.0[..4])`.
- **Do NOT** touch kd=ts (`maybe_emit_tally_share`, `reserve_next_local_hlc` at `:1466`) — per-committee-member shares are intentionally distinct.
- **Do NOT** change `reserve_next_local_hlc` itself — it stays for kd=ts and non-engine callers.

---

## File Structure

- `src-tauri/src/community_voting_log_engine.rs` — the helper (new, module-private), the mint-site switches, the `base_hlc` param + threading, the inbound re-enable.
- `src-tauri/src/community_voting_tier3.rs` — new `close_hlc` field on `Tier3PollState`, set in the `PollClose` apply arm.
- `src-tauri/tests/community_voting/community_voting_tier3_ipc_integration.rs` — strengthened race-tolerant test + new se-mode two-engine test.

---

## Task 1: Pure derivation helper `engine_auto_hlc_from_base`

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs` (add module-private fn near `reserve_next_local_hlc` ~:537; add a `#[cfg(test)]` unit test in the file's existing test module)

**Interfaces:**
- Produces: `fn engine_auto_hlc_from_base(base: &Hlc, pid: &PollId, kind: &str) -> Hlc` — used by Tasks 3 & 4.

- [ ] **Step 1: Write the failing unit test**

Add to the `#[cfg(test)] mod` in `community_voting_log_engine.rs` (use the existing test module; if none, add `#[cfg(test)] mod zeb316_hlc_tests { use super::*; ... }`):

```rust
#[test]
fn engine_auto_hlc_from_base_is_deterministic_and_strictly_newer() {
    let pid = PollId([0xAB; 32]);
    let base = Hlc { wall_ms: 1_000, logical: 3, device_id: "engine".into() };

    let a = engine_auto_hlc_from_base(&base, &pid, "cl");
    let b = engine_auto_hlc_from_base(&base, &pid, "cl");
    // Deterministic: identical (base, pid, kind) → identical HLC.
    assert_eq!(a, b);
    // Strictly newer than base.
    assert!(a.is_strictly_newer_than(&base), "must be strictly newer");
    // Poll-derived lane (first 4 bytes of poll_id hex).
    assert_eq!(a.device_id, "engine-auto-cl-abababab");
    // Same wall, logical+1 in the common case.
    assert_eq!(a.wall_ms, 1_000);
    assert_eq!(a.logical, 4);

    // Distinct kinds → distinct lanes, but both strictly newer than base.
    let rs = engine_auto_hlc_from_base(&base, &pid, "rs");
    assert_eq!(rs.device_id, "engine-auto-rs-abababab");
    assert!(rs.is_strictly_newer_than(&base));

    // Saturation guard: logical at u32::MAX bumps wall instead.
    let maxed = Hlc { wall_ms: 5, logical: u32::MAX, device_id: "x".into() };
    let d = engine_auto_hlc_from_base(&maxed, &pid, "cl");
    assert_eq!(d.wall_ms, 6);
    assert_eq!(d.logical, 0);
    assert!(d.is_strictly_newer_than(&maxed));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures --lib -E 'test(engine_auto_hlc_from_base_is_deterministic_and_strictly_newer)'`
Expected: FAIL — `engine_auto_hlc_from_base` not found.

- [ ] **Step 3: Implement the helper**

Add near `reserve_next_local_hlc` (~:537) in `community_voting_log_engine.rs`:

```rust
/// ZEB-316: deterministic, replica-identical HLC for an engine-auto mint.
///
/// Strictly newer than `base`; the `device_id` is a poll-derived lane
/// (`engine-auto-{kind}-{poll_prefix}`) so every replica reacting to the
/// SAME `base` produces a bit-identical HLC → bit-identical signing_bytes →
/// bit-identical event_hash. Unlike `reserve_next_local_hlc`, this reads NO
/// wall-clock, NO `self.device_id`, and does NOT touch the hlc_tracker — all
/// three diverge per replica. `kind` ∈ {"cl","sf","rs"}.
fn engine_auto_hlc_from_base(base: &Hlc, pid: &PollId, kind: &str) -> Hlc {
    let lane = format!("engine-auto-{kind}-{}", hex::encode(&pid.0[..4]));
    // Strictly newer by (wall_ms, logical, device_id): logical+1 at equal wall.
    // Saturation guard (astronomically unlikely — logical resets on wall advance):
    // if logical is maxed, bump wall and reset logical so it stays strictly newer.
    if base.logical == u32::MAX {
        Hlc { wall_ms: base.wall_ms.saturating_add(1), logical: 0, device_id: lane }
    } else {
        Hlc { wall_ms: base.wall_ms, logical: base.logical + 1, device_id: lane }
    }
}
```

Confirm `Hlc` and `PollId` are already in scope in this file (they are — used throughout). Add `use` only if the build complains.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures --lib -E 'test(engine_auto_hlc_from_base_is_deterministic_and_strictly_newer)'`
Expected: PASS.

- [ ] **Step 5: Gate + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --lib --features test-fixtures --no-deps -- -D warnings
git add src-tauri/src/community_voting_log_engine.rs
git commit -m "ZEB-316: add pure engine_auto_hlc_from_base derivation helper"
```

---

## Task 2: `close_hlc` state field on `Tier3PollState`

**Files:**
- Modify: `src-tauri/src/community_voting_tier3.rs` (add field ~:204, init ~:419, set in `PollClose` apply arm ~:1025)
- Test: add a `#[cfg(test)]` unit test in the same file (or the nearest existing tier3 test module)

**Interfaces:**
- Produces: `Tier3PollState.close_hlc: Option<Hlc>` — read by Tasks 3 & 4 (result mints).

- [ ] **Step 1: Write the failing test**

Add a unit test that builds a `Tier3PollState`, applies a `PollClose` event, and asserts `close_hlc` is populated from the event's HLC. Model it on the existing tier3 apply tests in this file (find one that applies a `PollClose` and reuse its fixture setup). The assertion:

```rust
// After applying the kd=cl `close_ev` (hlc = close_hlc_in):
assert_eq!(state.close_hlc, Some(close_hlc_in),
    "close_hlc must be set from the applied PollClose event's HLC");
assert!(state.close_event_hash.is_some());
```

If no close-apply unit test exists to copy, add one that: constructs a poll in Ratification stage, builds a signed `PollClose` via `build_signed_poll_close_tier3(&key, actor, pid, close_hlc_in)`, applies it, and asserts both `close_hlc` and `close_event_hash`.

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures --lib -E 'test(close_hlc)'`
Expected: FAIL — no `close_hlc` field.

- [ ] **Step 3: Add the field + init + set-site**

In `community_voting_tier3.rs`:

Field (add immediately after `close_event_hash: Option<[u8; 32]>,` at ~:204):
```rust
    /// ZEB-316: HLC of the applied kd=cl close event. Replica-canonical
    /// (the close is deterministic once minted), so engine-auto result mints
    /// (pu- and se-mode kd=rs) anchor to it for bit-identical events.
    pub close_hlc: Option<Hlc>,
```

Constructor init (add alongside `close_event_hash: None,` at ~:419):
```rust
            close_hlc: None,
```

Set-site — in the `PollClose` apply arm (~:1023-1026), alongside `self.close_event_hash = Some(hash);`:
```rust
            PollEventKindCode::PollClose => {
                let hash = sha256_of_signing_bytes(ev);
                self.close_event_hash = Some(hash);
                self.close_hlc = Some(ev.hlc.clone());
            }
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures --lib -E 'test(close_hlc)'`
Expected: PASS.

- [ ] **Step 5: Gate + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --lib --features test-fixtures --no-deps -- -D warnings
git add src-tauri/src/community_voting_tier3.rs
git commit -m "ZEB-316: record close_hlc on Tier3PollState at PollClose apply"
```

---

## Task 3: Switch engine-auto mints to deterministic HLC + thread base (inbound still disabled)

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs` — `maybe_trigger_engine_auto_orchestration` signature + kd=sf/kd=cl/pu-kd=rs mints; the local caller at ~:1831. (`try_finalize_secret_tally` se-mode kd=rs stays on wall-clock — C1 refinement, see Task 3 (e).)
- Test: `src-tauri/tests/community_voting/community_voting_tier3_ipc_integration.rs` — assert single-engine deterministic close_hlc.

**Interfaces:**
- Consumes: `engine_auto_hlc_from_base` (Task 1), `Tier3PollState.close_hlc` (Task 2).
- Produces: `maybe_trigger_engine_auto_orchestration(self, pid, base_hlc: &Hlc)` — the new signature Task 4's inbound caller uses.

- [ ] **Step 1: Write the failing determinism assertion**

In `ipc_tier3_engine_auto_kd_cl_kd_rs_race_tolerant` (~:964), after the `Stage::Finalized` waits and the `t3_a`/`t3_b` snapshot, add (engine_a mints via the local path in this task; both close_hlcs equal the deterministic derivation of the kd=ss trigger HLC):

```rust
    // ZEB-316: the engine-auto kd=cl HLC is deterministic — derived purely
    // from the triggering kd=ss HLC, not wall-clock. Both replicas converge
    // on the same close_hlc.
    let expected_close_hlc = /* mirror engine_auto_hlc_from_base(&ss_hlc, poll_id, "cl") */
        Hlc {
            wall_ms: ss_hlc_wall,
            logical: 1, // hlc_at(..) uses logical 0 → derived logical 1
            device_id: format!("engine-auto-cl-{}", hex::encode(&poll_id.0[..4])),
        };
    assert_eq!(t3_a.close_hlc.as_ref(), Some(&expected_close_hlc),
        "kd=cl HLC must be the deterministic derivation of the kd=ss trigger HLC");
    assert_eq!(t3_a.close_hlc, t3_b.close_hlc,
        "both replicas converge on the same close_hlc");
```

(Confirm `hlc_at` sets `logical: 0`; if it uses a different logical, adjust `expected_close_hlc.logical` to `<that logical> + 1`.)

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures --test community_voting -E 'test(ipc_tier3_engine_auto_kd_cl_kd_rs_race_tolerant)'`
Expected: FAIL — close_hlc is currently the wall-clock/`self.device_id` value, not the deterministic derivation.

- [ ] **Step 3: Switch the mint sites + thread the base**

In `community_voting_log_engine.rs`:

(a) Change the signature (~:872):
```rust
    async fn maybe_trigger_engine_auto_orchestration(self: &Arc<Self>, pid: &PollId, base_hlc: &Hlc) {
```

(b) kd=sf mint (~:941) — replace `let hlc = self.reserve_next_local_hlc().await;` with:
```rust
            let hlc = engine_auto_hlc_from_base(base_hlc, pid, "sf");
```

(c) kd=cl mint (~:1057) — replace with:
```rust
            let hlc = engine_auto_hlc_from_base(base_hlc, pid, "cl");
```

(d) pu-kd=rs mint (~:1161) — anchor to `close_hlc` read from state. In the kd=rs trigger-snapshot block (which already locks the log and reads `t3`), capture `let close_hlc = t3.close_hlc.clone();` and carry it out of the block; then replace the mint's `reserve_next_local_hlc` with:
```rust
            let Some(close_hlc) = close_hlc else {
                tracing::debug!(poll_id = %hex::encode(pid.0), "engine-auto kd=rs: close_hlc absent; skip");
                return;
            };
            let hlc = engine_auto_hlc_from_base(&close_hlc, pid, "rs");
```

(e) `try_finalize_secret_tally` se-mode kd=rs mint (~:1566) — **EXCLUDED from deterministic HLC (C1 refinement); keep `let hlc = self.reserve_next_local_hlc().await;` (wall-clock), do NOT anchor on `close_hlc`.** A deterministic-monotonic base is unachievable: committee kd=ts land AFTER the close at per-replica walls `> close_hlc.wall` and aren't replica-identical, so a close-anchored kd=rs is non-monotonic and rejected by the apply-time gate → the poll never finalizes via engine-auto. Determinism is unneeded anyway — the kd=rs *result* converges bit-identically via Lagrange invariance in `recover_secret_tally` + the apply-time LWW gate. (This is the pre-ZEB-316 wall-clock behavior; leave it in place. `close_hlc` remains snapshotted/used only by pu-kd=rs.)

(f) Local caller (~:1831) — thread the applied event's HLC:
```rust
            self.maybe_trigger_engine_auto_orchestration(&applied_poll_id, &event.hlc)
                .await;
```

Leave the inbound path (`process_inbound_dispatch` ~:2804) unchanged in this task.

- [ ] **Step 4: Run the determinism assertion + regressions**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --test community_voting -E 'test(ipc_tier3_engine_auto)'
```
Expected: PASS (deterministic close_hlc; existing convergence still holds — engine_a mints, propagates over the bridge).

- [ ] **Step 5: Gate + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --lib --features test-fixtures --no-deps -- -D warnings
git add src-tauri/src/community_voting_log_engine.rs src-tauri/tests/community_voting/community_voting_tier3_ipc_integration.rs
git commit -m "ZEB-316: derive engine-auto mint HLCs deterministically (base-threaded)"
```

---

## Task 4: Re-enable inbound orchestration + prove bit-identical independent mints

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs` — re-enable hook at `process_inbound_dispatch` (~:2804); reconcile the redundant se-mode-only block (~:2843).
- Test: `src-tauri/tests/community_voting/community_voting_tier3_ipc_integration.rs` — strengthen the race-tolerant test (independent mints now byte-identical), add a determinism-repeat, add an se-mode two-engine test.

**Interfaces:**
- Consumes: `maybe_trigger_engine_auto_orchestration(self, pid, base_hlc)` (Task 3).

- [ ] **Step 1: Re-enable the inbound hook**

At the NOTE site in `process_inbound_dispatch` (~:2792-2811), inside `if event.tier == Tier::Sortition`, add the orchestration call, matching the local path's ordering (orchestration BEFORE the lifecycle emit):
```rust
        if event.tier == Tier::Sortition {
            // ZEB-316: peer engines holding local_signing now auto-orchestrate
            // from the inbound path too. The mint HLC is derived deterministically
            // from `event.hlc` (this applied trigger, byte-identical on every
            // replica), so independent peer mints are bit-identical → trivial LWW.
            self.maybe_trigger_engine_auto_orchestration(&applied_poll_id, &event.hlc)
                .await;
            self.maybe_emit_tier3_lifecycle_events(
                &applied_poll_id, &event, previous_stage_for_emit,
            )
            .await;
        }
```
Replace the `NOTE: … deliberately NOT called` comment block accordingly.

- [ ] **Step 2: Reconcile the redundant se-mode block (~:2843)**

The re-enabled cascade's tail already runs `maybe_emit_tally_share` + `try_finalize_secret_tally` for every Tier-3 inbound event (both internally gate on privacy_mode/committee state). The standalone se-mode block at ~:2843 (gated on `TallyShare|PollClose`) is now subsumed. **Remove that block** and its now-stale explanatory comment. (Its behavior is preserved: the cascade calls the same two methods.)

- [ ] **Step 3: Strengthen the race-tolerant test**

Now both engines mint independently (engine_b via the re-enabled inbound path). The existing `close_event_hash_a == close_event_hash_b` assertion now proves byte-identical *independent* mints. Add a stronger, pre-computed check that both equal the deterministic value, and keep the Task-3 `close_hlc` assertions. Then add a determinism-repeat wrapper test that runs the whole two-engine scenario N times and asserts the same close_event_hash every iteration:

```rust
#[tokio::test]
async fn ipc_tier3_engine_auto_kd_cl_kd_rs_deterministic_repeat() {
    // Run the race-tolerant scenario many times; a wall-clock-derived HLC
    // would make close_event_hash vary run-to-run. Structural determinism
    // ⇒ identical every iteration.
    let mut hashes = std::collections::HashSet::new();
    for _ in 0..100 {
        let h = run_race_tolerant_and_return_close_hash().await; // extract the scenario into a helper
        hashes.insert(h);
    }
    assert_eq!(hashes.len(), 1, "close_event_hash must be identical across 100 runs");
}
```
Refactor the body of `ipc_tier3_engine_auto_kd_cl_kd_rs_race_tolerant` into `run_race_tolerant_and_return_close_hash()` returning the finalized `close_event_hash`, and have the original test assert on it too (DRY).

- [ ] **Step 4: Add an se-mode two-engine result-convergence test** (C1 refinement)

Model on the race-tolerant test but with `privacy_mode: "se"`, a threshold committee, both engines crossing the ≥t share threshold. Publish the committee kd=ts at walls **strictly greater than the close** (`kd=ts.wall > close.wall` — the realistic production arrangement) and assert both engines reach `Finalized` with an **identical recovered `StarResult`**. Do NOT assert the se-mode kd=rs `event_hash`/HLC or `close_hlc` is byte-identical across engines — se-mode kd=rs is intentionally wall-clock (non-deterministic HLC); convergence is on the *result* only, via Lagrange invariance + the LWW gate. This test is the C1 RED→GREEN evidence: it FAILS against the pre-fix close-anchored se-mode kd=rs (engine won't finalize — the close-anchored kd=rs is non-monotonic once a higher-wall kd=ts is applied) and PASSES after reverting se-mode kd=rs to wall-clock. Drive finalization through the re-enabled inbound cascade (engine_a as no-signing-key relay, engine_b mints). If a suitable se-mode two-engine bridge fixture exists in this file, reuse it; otherwise extend `setup_two_voting_engine_bridge`. Keep committee `t` small (e.g. 2) to bound cost.

- [ ] **Step 5: Run the full voting integration suite**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --test community_voting
```
Expected: PASS — including the strengthened race-tolerant test, the 100× repeat, and the se-mode test. If any pre-existing voting test regresses from the inbound re-enable, investigate (it likely encodes an assumption the re-enable changes) before proceeding.

- [ ] **Step 6: Final full CI-parity gate + commit**

```bash
cd src-tauri && cargo fmt --all -- --check \
  && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings \
  && cargo nextest run --locked --workspace --all-targets --features test-fixtures
git add -A
git commit -m "ZEB-316: re-enable inbound engine-auto orchestration (deterministic mints)"
```

---

## Self-Review Notes

- **Spec coverage:** Task 1 = the helper; Task 2 = `close_hlc`; Task 3 = **three** mint switches (kd=sf, kd=cl, pu-kd=rs) + base threading + local caller; Task 4 = inbound re-enable + the 3 acceptance criteria (bit-identical convergence, hook re-enabled, deterministic 100× pass). kd=ts and se-mode kd=rs (C1 refinement) explicitly untouched (stay on wall-clock).
- **Determinism proof lives in Task 4** (two engines mint independently); Task 3's single-engine `close_hlc` assertion proves the derivation is wired at the mint site.
- **Type consistency:** helper signature `engine_auto_hlc_from_base(&Hlc, &PollId, &str) -> Hlc` is stable across Tasks 1/3/4; `close_hlc: Option<Hlc>` stable across Tasks 2/3/4; hook signature `(…, base_hlc: &Hlc)` stable across Tasks 3/4.
- **Open naming item** (from spec): `engine_auto_hlc_from_base` chosen over the ticket's `reserve_next_local_hlc_from_base` (no reservation happens). Flag in PR description.
