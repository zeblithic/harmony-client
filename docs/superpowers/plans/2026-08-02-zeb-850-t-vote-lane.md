# ZEB-850 T-VOTE-LANE Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the tier-3 voting GRIEF-LOCKOUT (a peer's future wall stamp freezes a poll) by re-keying the receive watermark per-`(actor,device)`, and close the peer-ingest authorization bypass by enforcing the (currently zero-caller) authz verifiers at the admission seam.

**Architecture:** Three bundled fixes at the tier-3 layer. (A) `Tier3PollState.last_received_hlc` goes from a single `Option<Hlc>` global to a per-`(OwnerAddr, String)` `BTreeMap` lane, mirroring the proven ZEB-585 `WatermarkVector`; the monotonic guard becomes per-lane. (B) `inbound_eligibility_check` enforces `verify_sf/sd/da/rb/sr` (sync) and `verify_ss` (async, `BeaconOracle`, fail-closed) on both peer routes (`process_inbound`, `apply_backfilled_event`). (C) A discrimination test pins the existing ZEB-846 E1 clamp.

**Tech Stack:** Rust, `chrono` 0.4, `tokio::sync::Mutex`, existing `clock_trust` policy module, `BTreeMap`.

## Global Constraints

- **CI gates (run from `src-tauri/`):** `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. Iterative dev may scope with `-p harmony-app --lib` / `scripts/test-select`, but the final gate is the full `--all-targets` sweep.
- **MSRV = 1.91** — no newer std APIs.
- **No wire/schema change.** All edits are in-memory state + ingest gating. `MINT_SCHEMA_VERSION`/voting wire formats are untouched.
- **Control tier only:** any clock bound uses `clock_trust::MAX_FORWARD_SKEW_MS` (5 min). Never the display tier.
- **`last_hlc` is NOT changed** — it is the accepted-only projection watermark and is correctly a single global scalar. Only `last_received_hlc` is re-keyed.
- **Preserve ZEB-320 / Qodo-#154:** the per-lane guard must still reject an earlier-HLC event *within the same `(actor,device)` lane* after a dropped event.
- **`verify_ss` lock discipline:** never hold the `voting_log` guard across the `verify_ss` await (it locks the dfrost log internally — ZEB-803 cross-lock class). Clone the `Tier3PollState` under the guard, drop the guard, then await.
- **`verify_ss` fail-closed:** on `BeaconNotYetAvailable` or a missing oracle, drop the peer `kd=ss` (liveness-safe: `kd=ss` is engine-auto-derived locally from the beacon).
- **Discrimination tests:** every new test must fail if its gate is neutralized.
- Commit messages end with the standard `Co-Authored-By` + `Claude-Session` trailers.

---

### Task 1: E2 — per-(actor,device) receive-watermark re-key

**Files:**
- Modify: `src-tauri/src/community_voting_tier3.rs` (field, init, guard, write, helper; tests)
- Modify: `src-tauri/src/community_voting_log_engine.rs:1246`, `:1725` (mint-floor consumers)

**Interfaces:**
- Consumes: `OwnerAddr` (`owner_state_types.rs:411`, `Copy+Ord+Hash`), `Hlc` (`owner_state_types.rs:348`, fields `wall_ms:u64, logical:u32, device_id:String`), `ev.actor: OwnerAddr`, `ev.hlc: Hlc`.
- Produces: `Tier3PollState.last_received_hlc: BTreeMap<(OwnerAddr, String), (u64, u32)>`; new method `Tier3PollState::max_received_hlc() -> Option<Hlc>`.

- [ ] **Step 1: Write the failing lane-isolation test**

In `community_voting_tier3.rs`'s `#[cfg(test)] mod tests`, add a test that builds two tier-3 events for the same poll from **different** `(actor, device_id)` lanes: device A stamps a far-future `wall_ms` (e.g. `now + 3_600_000`), device B stamps `now`. Apply A, then apply B, then apply a second B event at `now + 1`. Assert both B applies return `Ok(())` (B's lane is unaffected by A's future watermark). Use existing test helpers/fixtures in this module (search for an existing `apply_event` test such as `guard_still_rejects_earlier_hlc_after_dropped_event` at ~`:4488` for the event-construction pattern — reuse its signing/HLC helpers). Pick a kind that materializes without extra preconditions, e.g. `kd=md` (MiniPublicDecline) with the actors placed in the mini-public, OR assert on the guard directly via two events whose only difference is the lane.

```rust
#[test]
fn future_event_on_one_lane_does_not_stall_another_lane() {
    // device A: future wall on lane (actor_a, "devA"); device B: honest now on (actor_b, "devB").
    // Apply A (accepted or dropped — irrelevant), then B@now, then B@now+1.
    // Both B applies must be Ok — under the OLD global watermark the first B
    // would be HlcNotMonotonic.
    // (construct via the module's existing event/HLC helpers)
}
```

- [ ] **Step 2: Run it to verify it fails (compile error — field is still `Option<Hlc>`)**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(future_event_on_one_lane)'`
Expected: FAIL (either a compile error if the test pokes the map, or `HlcNotMonotonic` on the second B apply under the current global watermark).

- [ ] **Step 3: Change the field type + init**

`community_voting_tier3.rs:222` — replace the field:
```rust
    /// Per-`(actor, device_id)` receive-watermark lanes (ZEB-850, mirrors the
    /// channel-log `WatermarkVector`, ZEB-585). Each lane holds the highest
    /// `(wall_ms, logical)` dispatched on that lane — accepted OR silently
    /// dropped (the ZEB-320 property, now per-lane). Keying per lane stops one
    /// sibling's future-stamped event from freezing every other member's
    /// events (the T-VOTE-LANE GRIEF-LOCKOUT). Rebuilt by replay; never
    /// serialized (community_voting_persist.rs).
    pub last_received_hlc: std::collections::BTreeMap<(OwnerAddr, String), (u64, u32)>,
```
`community_voting_tier3.rs:423` — replace the init in `new_from_create`:
```rust
            last_received_hlc: std::collections::BTreeMap::new(),
```
Also update the hand-rolled `Debug` impl field (`:255`) — it can print the map directly:
```rust
            .field("last_received_hlc", &self.last_received_hlc)
```
(unchanged line text; verify it still compiles against the new type — `BTreeMap` is `Debug`).

- [ ] **Step 4: Rewrite the monotonic guard (per-lane)**

`community_voting_tier3.rs:457-467` — replace the guard block:
```rust
        // ZEB-850: per-(actor, device) monotonic guard. Compare the incoming
        // event only against the high-water mark of ITS OWN lane. device_id is
        // constant within a lane, so comparing (wall_ms, logical) is equivalent
        // to the old (wall_ms, logical, device_id) 3-tuple compare. A future
        // event on one lane no longer blocks another lane's honest events.
        let lane = (ev.actor, ev.hlc.device_id.clone());
        if let Some(&(w, l)) = self.last_received_hlc.get(&lane) {
            if (ev.hlc.wall_ms, ev.hlc.logical) < (w, l) {
                return Err(ApplyError::HlcNotMonotonic);
            }
        }
```

- [ ] **Step 5: Rewrite the watermark write (per-lane max-raise)**

`community_voting_tier3.rs:1052` — replace `self.last_received_hlc = Some(ev.hlc.clone());`:
```rust
        // Raise this event's (actor, device) lane to max((wall_ms, logical)),
        // mirroring channel-log raise_watermark. Advances on every Ok (accept
        // or silent drop) — the ZEB-320 property, per lane.
        let lane = (ev.actor, ev.hlc.device_id.clone());
        let cand = (ev.hlc.wall_ms, ev.hlc.logical);
        let entry = self.last_received_hlc.entry(lane).or_insert((0, 0));
        if cand > *entry {
            *entry = cand;
        }
```

- [ ] **Step 6: Add the `max_received_hlc` helper**

Add a method on `Tier3PollState` (near `current_stage_at`, e.g. after `:1057`):
```rust
    /// Highest `(wall_ms, logical)` across all per-(actor,device) receive
    /// lanes, as an `Hlc` with an empty `device_id`, or `None` before any
    /// dispatch. The engine-auto kd=rs mint floor needs a floor strictly above
    /// EVERY received event regardless of lane; `engine_auto_hlc_from_base`
    /// synthesizes its own device_id, so the empty one here is never used.
    pub fn max_received_hlc(&self) -> Option<Hlc> {
        self.last_received_hlc
            .values()
            .copied()
            .max()
            .map(|(wall_ms, logical)| Hlc {
                wall_ms,
                logical,
                device_id: String::new(),
            })
    }
```
(Confirm `Hlc` is in scope in this file — it is used throughout, e.g. `:458`.)

- [ ] **Step 7: Fix the two kd=rs mint-floor consumers**

`community_voting_log_engine.rs:1246` — replace `let last_received = t3.last_received_hlc.clone();`:
```rust
                let last_received = t3.max_received_hlc();
```
`community_voting_log_engine.rs:1725` — the identical se-mode line, same replacement:
```rust
                let last_received = t3.max_received_hlc();
```
No other change is needed at either mint site: `last_received` stays `Option<Hlc>`, and the downstream `let base = match last_received.as_ref() { Some(w) => w, ... }` → `engine_auto_hlc_from_base(base, pid, "rs")` (`:1324-1338`, `:1801`) consumes `&Hlc` exactly as before (the synthesized empty `device_id` is ignored by `engine_auto_hlc_from_base`).

- [ ] **Step 8: Update existing tests that read the old field shape**

Search this module for direct reads of `last_received_hlc` as `Option<Hlc>` and adapt:
- `~:4503` `poll.last_received_hlc.as_ref().unwrap().wall_ms` → `poll.max_received_hlc().unwrap().wall_ms`.
- `~:4885` `state.last_received_hlc.as_ref().map(|h| h.wall_ms)` (assert "must advance on every dispatch") → `state.max_received_hlc().map(|h| h.wall_ms)`.
- `guard_still_rejects_earlier_hlc_after_dropped_event` (`~:4488`): ensure the two events it applies share the **same** `(actor, device_id)` lane (same `ev.actor` and same `ev.hlc.device_id`) so the per-lane guard still rejects the earlier-HLC second event. If the fixture already uses one actor+device, it passes unchanged; if not, pin the device_id equal.

- [ ] **Step 9: Add the within-lane (#154) preservation test + mint-floor-over-lanes test**

```rust
#[test]
fn within_lane_earlier_hlc_still_rejected_after_dropped_event() {
    // Same (actor, device): dispatch at (wall=10, logical=0), then (wall=5).
    // The second must return HlcNotMonotonic — the ZEB-320/#154 property holds
    // per lane. (Neutralizing the guard makes this pass → test fails.)
}

#[test]
fn max_received_hlc_is_max_over_lanes() {
    // Dispatch (actor_a,"d1")@wall=100 and (actor_b,"d2")@wall=200.
    // max_received_hlc().unwrap().wall_ms == 200 (not lane-order dependent).
}
```

- [ ] **Step 10: Gate + commit**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(lane) + test(max_received_hlc) + test(guard_still_rejects) + test(within_lane)'` then `cargo clippy --locked -p harmony-app --all-targets --features test-fixtures --no-deps -- -D warnings` and `cargo fmt --all -- --check`.
Expected: all PASS/clean.
```bash
git add src-tauri/src/community_voting_tier3.rs src-tauri/src/community_voting_log_engine.rs
git commit -m "ZEB-850 Task 1: per-(actor,device) tier-3 receive-watermark lane"
```

---

### Task 2: authz enforcement — sync verifiers (sf/md/dc/da/rb/rs)

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs` (`inbound_eligibility_check` Sortition arm; add a `with_tier3` helper; tests)

**Interfaces:**
- Consumes: `verify_sf`/`verify_sd`/`verify_da_candidate_exists`/`verify_ratification_ballot`/`verify_sr` (all sync, `(event, &Tier3PollState) -> Result<(), VerifyError>`, `community_voting_tier3.rs:1316-1420`); `decode_poll_id_ref(&[u8]) -> Option<PollId>` (`community_voting_log.rs:767`); `TierState::as_tier3() -> Option<&Tier3PollState>` (`community_voting_log.rs:139`).
- Produces: gated ingest for `kd=sf/md/dc/da/rb/rs`. (`kd=ss` is added in Task 3.)

- [ ] **Step 1: Write a failing forge-kd=sf test**

Add a test that ingests a `kd=sf` (SortitionFailed) event from a **non-proposer** member through `inbound_eligibility_check` (construct a `MembershipSnapshot`, a tier-3 poll in the log via the module's fixtures, and a signed non-proposer `kd=sf`). Assert `inbound_eligibility_check(...).await` returns `Err` (authz reject). Search the module for an existing eligibility-check test (e.g. around the Tier1 BallotCast tests) for the fixture pattern.

```rust
#[tokio::test]
async fn kd_sf_from_non_proposer_rejected_at_ingest() {
    // poll proposer = P; event actor = X != P → verify_sf → SfActorNotProposer
    // inbound_eligibility_check(...).await must be Err.
}
```

- [ ] **Step 2: Run it, verify it fails (currently no-op → Ok)**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(kd_sf_from_non_proposer)'`
Expected: FAIL (today the `_ => {}` / engine-auto arm returns `Ok`).

- [ ] **Step 3: Add a `with_tier3` helper (DRY the lookup + sync verify)**

Add above `inbound_eligibility_check` in `community_voting_log_engine.rs`:
```rust
/// Look up a tier-3 poll's `Tier3PollState` under the log guard and run a
/// SYNC authz verifier against it. Holds the guard only across the sync check
/// (no await → no cross-lock hazard). Maps any `VerifyError` and an
/// unknown/non-tier3 poll to a rejection string.
async fn with_tier3<F>(
    voting_log: &Arc<Mutex<VotingLog>>,
    pid: &PollId,
    kind: &str,
    f: F,
) -> Result<(), String>
where
    F: FnOnce(
        &crate::community_voting_tier3::Tier3PollState,
    ) -> Result<(), crate::community_voting_tier3::VerifyError>,
{
    let log_g = voting_log.lock().await;
    let t3 = log_g
        .polls
        .get(pid)
        .and_then(|ps| ps.tier_state.as_tier3())
        .ok_or_else(|| format!("{kind} authz: unknown/non-tier3 poll {}", hex::encode(pid.0)))?;
    f(t3).map_err(|e| format!("{kind} authz: {e:?}"))
}
```

- [ ] **Step 4: Restructure the `Tier::Sortition` match arm**

`community_voting_log_engine.rs:3341-3353` — replace the engine-auto no-op line and the `_ => {}` line. Keep the `PollCreate` arm (`:3323-3335`) unchanged. New arms (leave `SortitionSelection` as a no-op here — Task 3 gates it):
```rust
                // kd=cl (PollClose) is engine-auto with no authz verifier
                // (no verify_cl exists); membership-V6 is the outer gate.
                // kd=ss is gated in ZEB-850 Task 3 (async, BeaconOracle) — it
                // stays a no-op here until then.
                crate::community_voting_core::PollEventKindCode::SortitionSelection
                | crate::community_voting_core::PollEventKindCode::PollClose => {}
                // kd=sf: else any member could forge Stage::Failed and kill the
                // poll. verify_sf: proposer-signed + backup pool exhausted.
                crate::community_voting_core::PollEventKindCode::SortitionFailed => {
                    let pid = crate::community_voting_log::decode_poll_id_ref(&event.payload)
                        .ok_or_else(|| "kd=sf: undecodable poll id".to_string())?;
                    with_tier3(voting_log, &pid, "kd=sf", |t3| {
                        crate::community_voting_tier3::verify_sf(event, t3)
                    })
                    .await?;
                }
                // kd=rs: else a member could forge an arbitrary finalized result.
                // verify_sr: kd=cl applied + tally bit-identical to recompute.
                crate::community_voting_core::PollEventKindCode::PollResult => {
                    let pid = crate::community_voting_log::decode_poll_id_ref(&event.payload)
                        .ok_or_else(|| "kd=rs: undecodable poll id".to_string())?;
                    with_tier3(voting_log, &pid, "kd=rs", |t3| {
                        crate::community_voting_tier3::verify_sr(event, t3)
                    })
                    .await?;
                }
                // kd=md / kd=dc: mini-public membership (verify_sd).
                crate::community_voting_core::PollEventKindCode::MiniPublicDecline
                | crate::community_voting_core::PollEventKindCode::DraftCandidate => {
                    let pid = crate::community_voting_log::decode_poll_id_ref(&event.payload)
                        .ok_or_else(|| "kd=md/dc: undecodable poll id".to_string())?;
                    with_tier3(voting_log, &pid, "kd=md/dc", |t3| {
                        crate::community_voting_tier3::verify_sd(event, t3)
                    })
                    .await?;
                }
                // kd=da: membership + referenced candidate must exist.
                crate::community_voting_core::PollEventKindCode::DraftApproval => {
                    let pid = crate::community_voting_log::decode_poll_id_ref(&event.payload)
                        .ok_or_else(|| "kd=da: undecodable poll id".to_string())?;
                    with_tier3(voting_log, &pid, "kd=da", |t3| {
                        crate::community_voting_tier3::verify_sd(event, t3)?;
                        crate::community_voting_tier3::verify_da_candidate_exists(event, t3)
                    })
                    .await?;
                }
                // kd=rb: crypto is checked at apply; add B3 electorate authz.
                crate::community_voting_core::PollEventKindCode::RatificationBallot => {
                    let pid = crate::community_voting_log::decode_poll_id_ref(&event.payload)
                        .ok_or_else(|| "kd=rb: undecodable poll id".to_string())?;
                    with_tier3(voting_log, &pid, "kd=rb", |t3| {
                        crate::community_voting_tier3::verify_ratification_ballot(event, t3)
                    })
                    .await?;
                }
                // kd=ds / kd=dv already inline-check mini-public membership in
                // apply_event (community_voting_tier3.rs:507/585).
                _ => {}
```
Ensure `VerifyError` is reachable (it is `pub`, referenced as `crate::community_voting_tier3::VerifyError` in the helper). Ensure `verify_sd`, `verify_sf`, `verify_sr`, `verify_da_candidate_exists`, `verify_ratification_ballot` are `pub` (they are, `:1316-1420`).

- [ ] **Step 5: Run the forge test — verify it passes; add the happy-path controls**

Run the Step-1 test → PASS. Add controls proving no over-rejection:
```rust
#[tokio::test]
async fn kd_sf_from_proposer_with_exhausted_pool_admitted() { /* verify_sf Ok → check returns Ok */ }
#[tokio::test]
async fn kd_md_from_non_mini_public_rejected() { /* verify_sd → NotInMiniPublic → Err */ }
#[tokio::test]
async fn kd_md_from_mini_public_member_admitted() { /* Ok */ }
#[tokio::test]
async fn kd_rs_before_close_rejected() { /* close_event_hash None → NotInClosedStage → Err */ }
```

- [ ] **Step 6: Gate + commit**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(kd_sf) + test(kd_md) + test(kd_rs_before_close) + test(inbound_eligibility)'`, then clippy `--all-targets` + fmt.
```bash
git add src-tauri/src/community_voting_log_engine.rs
git commit -m "ZEB-850 Task 2: enforce sync tier-3 authz verifiers at peer ingest"
```

---

### Task 3: authz enforcement — `verify_ss` (async, BeaconOracle, fail-closed)

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs` (`inbound_eligibility_check` + `process_inbound` signatures; `process_inbound_dispatch`, `apply_backfilled_event`, the test shim call sites; the `kd=ss` arm; tests)

**Interfaces:**
- Consumes: `verify_ss(event, &Tier3PollState, &dyn BeaconOracle, &SpaceId) -> Result<(), VerifyError>` (async, `community_voting_tier3.rs:1270`); `BeaconOracle` trait + `DfrostBeaconOracle<R> { registry: Arc<DfrostLogRegistry<R>> }` (`:1903`); `self.dfrost_registry: Mutex<Option<Arc<DfrostLogRegistry<R>>>>` (`:356`); `Tier3PollState: Clone` (`:189`).
- Produces: `kd=ss` gated at ingest, fail-closed.

- [ ] **Step 1: Write a failing forged-kd=ss test**

Add a test that, with a mock beacon oracle returning a known VRF output, ingests a `kd=ss` whose `primary`/`backup` do **not** match the deterministic sortition recompute. Assert `inbound_eligibility_check(...)` returns `Err` (`SortitionMismatch`). `MockBeaconOracle` (`community_voting_tier3.rs:3080`) is private to *that* file's `#[cfg(test)]` module, so define a **local** mock in `community_voting_log_engine.rs`'s test module (the `BeaconOracle` trait is `pub`):
```rust
struct FixedBeacon(Option<[u8; 32]>);
#[async_trait::async_trait]
impl crate::community_voting_tier3::BeaconOracle for FixedBeacon {
    async fn vrf_output_for(
        &self,
        _c: &crate::owner_state_types::SpaceId,
        _s: &[u8; 32],
        _e: u64,
    ) -> Option<[u8; 32]> {
        self.0
    }
}
```
To make a *matching* (admitted) `kd=ss`, compute the expected sortition via the same `fisher_yates_select` the verifier uses over a known VRF output; to make a mismatched one, perturb `primary`. Also add a fail-closed test: with `beacon_oracle = None` (or `FixedBeacon(None)` → `BeaconNotYetAvailable`), a `kd=ss` is rejected and `sortition_result` is never set.

```rust
#[tokio::test]
async fn kd_ss_mismatched_sortition_rejected() { /* oracle Some, wrong primary → SortitionMismatch → Err */ }
#[tokio::test]
async fn kd_ss_no_oracle_fail_closed() { /* beacon_oracle None → Err (fail-closed) */ }
```

- [ ] **Step 2: Run, verify failure (compile error — signatures lack the oracle/community_id)**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(kd_ss)'`
Expected: FAIL (compile — the new params don't exist yet).

- [ ] **Step 3: Extend `inbound_eligibility_check` signature**

`community_voting_log_engine.rs:3193` — add two params:
```rust
async fn inbound_eligibility_check(
    event: &SignedVotingEvent,
    snapshot: &crate::community_voting_core::MembershipSnapshot,
    voting_log: &Arc<Mutex<VotingLog>>,
    community_id: SpaceId,
    beacon_oracle: Option<&dyn crate::community_voting_tier3::BeaconOracle>,
) -> Result<(), String> {
```

- [ ] **Step 4: Add the `kd=ss` arm (clone → drop guard → await; fail-closed)**

In the `Tier::Sortition` match, **move** `SortitionSelection` out of the Task-2 `SortitionSelection | PollClose => {}` line (leave `PollClose => {}` alone) and add:
```rust
                // kd=ss: else a member could install a chosen mini-public,
                // whose forged members then pass the ds/dv inline checks.
                // verify_ss recomputes the sortition from the VRF beacon.
                // Clone the poll state under the guard, DROP the guard, then
                // await verify_ss (it locks the dfrost log internally — never
                // hold voting_log across that await, ZEB-803 cross-lock class).
                crate::community_voting_core::PollEventKindCode::SortitionSelection => {
                    let pid = crate::community_voting_log::decode_poll_id_ref(&event.payload)
                        .ok_or_else(|| "kd=ss: undecodable poll id".to_string())?;
                    let t3 = {
                        let log_g = voting_log.lock().await;
                        log_g
                            .polls
                            .get(&pid)
                            .and_then(|ps| ps.tier_state.as_tier3())
                            .cloned()
                            .ok_or_else(|| {
                                format!("kd=ss authz: unknown/non-tier3 poll {}", hex::encode(pid.0))
                            })?
                    };
                    // Fail-closed: no oracle wired ⇒ drop (liveness-safe, this
                    // node re-derives kd=ss from its own beacon).
                    let oracle = beacon_oracle
                        .ok_or_else(|| "kd=ss authz: no beacon oracle (fail-closed)".to_string())?;
                    crate::community_voting_tier3::verify_ss(event, &t3, oracle, &community_id)
                        .await
                        .map_err(|e| format!("kd=ss authz: {e:?}"))?;
                }
```
(`verify_ss` returns `BeaconNotYetAvailable` when the beacon isn't indexed → the `map_err` turns it into a reject = fail-closed, as designed.)

- [ ] **Step 5: Extend `process_inbound` signature + forward the oracle**

`community_voting_log_engine.rs:2770` — add the param after `floor`:
```rust
        floor: &crate::hlc_adopt_floor::HlcAdoptFloor,
        beacon_oracle: Option<&dyn crate::community_voting_tier3::BeaconOracle>,
        packet: &[u8],
```
`:2852` — pass both new args (community_id is already a param here):
```rust
        inbound_eligibility_check(&event, &snapshot, voting_log, community_id, beacon_oracle).await?;
```

- [ ] **Step 6: Build the oracle at the two `&self` call sites**

Add a private helper on the engine impl (near `apply_backfilled_event`):
```rust
    /// Build a `DfrostBeaconOracle` from the wired dfrost registry (if any),
    /// for the kd=ss ingest authz check. `None` ⇒ fail-closed at the seam.
    async fn beacon_oracle_holder(
        &self,
    ) -> Option<crate::community_voting_tier3::DfrostBeaconOracle<R>> {
        let reg_g = self.dfrost_registry.lock().await;
        reg_g
            .as_ref()
            .map(|r| crate::community_voting_tier3::DfrostBeaconOracle { registry: r.clone() })
    }
```
`process_inbound_dispatch` (`:3062` call): before the `Self::process_inbound(...)` call, build the holder and pass a trait-object ref:
```rust
        let oracle_holder = self.beacon_oracle_holder().await;
        let beacon_oracle = oracle_holder
            .as_ref()
            .map(|o| o as &dyn crate::community_voting_tier3::BeaconOracle);
        let applied = Self::process_inbound(
            self.community_id,
            &self.voting_log,
            &self.tracker,
            self.identity_resolver.as_ref(),
            self.membership_resolver.as_ref(),
            &self.adopt_floor,
            beacon_oracle,
            packet,
        )
        .await?;
```
`apply_backfilled_event` (`:2962` call): build the holder and pass it + `self.community_id`:
```rust
        let oracle_holder = self.beacon_oracle_holder().await;
        let beacon_oracle = oracle_holder
            .as_ref()
            .map(|o| o as &dyn crate::community_voting_tier3::BeaconOracle);
        inbound_eligibility_check(&event, &snapshot, &self.voting_log, self.community_id, beacon_oracle)
            .await?;
```

- [ ] **Step 7: Update the `process_inbound` test shim + test callers**

The test shim at `community_voting_log_engine.rs:3417` (`VotingLogEngine::<tauri::Wry>::process_inbound(...)`) and every `#[cfg(test)]` caller (`~:4990, :5127, :5216, :5295, :5371`) must pass the new `beacon_oracle` arg. Existing callers that don't exercise `kd=ss` pass `None`:
```rust
        &floor,
        None, // beacon_oracle — kd=ss authz not exercised here
        packet,
```
New `kd=ss` tests (Step 1) construct the local `FixedBeacon` and pass `Some(&oracle as &dyn crate::community_voting_tier3::BeaconOracle)` (either to `process_inbound` or directly to `inbound_eligibility_check`).

- [ ] **Step 8: Run the ss tests + gate**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(kd_ss)'` → PASS. Then full-module: `-E 'test(inbound_eligibility) + test(process_inbound) + test(kd_)'`, then clippy `--all-targets` + fmt.

- [ ] **Step 9: Commit**

```bash
git add src-tauri/src/community_voting_log_engine.rs
git commit -m "ZEB-850 Task 3: enforce verify_ss at peer ingest (fail-closed, guard-dropped)"
```

---

### Task 4: E1 — pin the ZEB-846 engine-trigger clamp

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs` (test only)

**Interfaces:**
- Consumes: the existing kd=cl auto-trigger clamp at `:1123-1135` (`clock_trust::clamp_future(last_wall, receiver_now_ms, MAX_FORWARD_SKEW_MS)`), `current_stage_at`.

- [ ] **Step 1: Write a discrimination test for the clamp**

Add a test that drives the kd=cl auto-trigger path with a `Tier3PollState` whose `last_hlc.wall_ms` is far-future (e.g. `receiver_now + 1h`) but whose poll windows would only elapse at a much later wall. Assert the stage the trigger computes is bounded by `receiver_now + MAX_FORWARD_SKEW_MS` (i.e. the future `last_hlc` does NOT advance the auto-computed stage to Ratification/Finalized). If the module exposes the trigger only via a higher-level entry, drive it through the smallest reachable seam (search for existing kd=cl trigger tests, e.g. near `voting_engine_*` tests) and assert on the resulting stage/no-premature-close. Neutralizing the `clamp_future` call must make this test fail.

```rust
#[tokio::test]
async fn e1_kd_cl_trigger_clamps_future_last_hlc_to_control_tier() {
    // last_hlc.wall_ms = receiver_now + 3_600_000 (1h). The kd=cl auto-trigger
    // must clamp "now" to receiver_now + MAX_FORWARD_SKEW_MS before
    // current_stage_at, so a poll whose windows elapse only after +5min does
    // NOT get force-advanced/closed by the poisoned watermark.
}
```

- [ ] **Step 2: Run — expect PASS (behavior already correct; the test PINS it)**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(e1_kd_cl_trigger_clamps)'`
Expected: PASS. Then temporarily neutralize the clamp locally (replace `clamp_future(...)` with `last_wall`) and re-run to confirm the test FAILS; restore the clamp.

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/community_voting_log_engine.rs
git commit -m "ZEB-850 Task 4: pin the ZEB-846 E1 engine-trigger clamp with a discrimination test"
```

---

## Final verification (controller, after all tasks)

From `src-tauri/`:
```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
All green before opening the PR. The whole-branch review checks: per-lane guard preserves ZEB-320/#154 within-lane; no `voting_log`-across-`verify_ss`-await; fail-closed on `BeaconNotYetAvailable`; `last_hlc` untouched; no wire/schema change; every discrimination test fails with its gate neutralized.
