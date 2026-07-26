# ZEB-813 Announce Supersession Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deterministic supersession of stale Reachability/CommunityRelay announces in the community CRDT log, so every replica compacts identically at insert/merge/load and over-cap communities heal on first boot.

**Architecture:** A defaulted `supersedes` method on core `LogPolicy` + a `Superseded` insert outcome; `VerifiedLog::insert` drops stale candidates on arrival and retains-out superseded events on insert; `from_verified_events` compacts at load. The client's `MembershipPolicy` implements the rule for the two announce kinds only; `encode_root_packet` gains size-watermark surfacing.

**Tech Stack:** Rust; core crate `harmony-crdt-sync` (no_std + alloc — `core::`/`alloc::` imports only); client `harmony-app` (src-tauri).

**Spec:** `docs/superpowers/specs/2026-07-26-zeb-813-announce-supersession-design.md` — its §1 contract clauses (a)–(d) are normative for Task 1.

## Global Constraints

- Core crate is `#![no_std]` + alloc: use `core::cmp::Ordering`, `alloc::vec::Vec`; no `std::`.
- Client cargo runs from `src-tauri/`; gates: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, nextest with `--locked --features test-fixtures`.
- ALL harmony crates in client `Cargo.toml` share ONE git rev — bump in lockstep, never split.
- Supersession applies ONLY to `ReachabilityAnnounce` (key: `actor` + `payload.iroh_node_id`) and `CommunityRelayAnnounce` (key: `actor` + `payload.relay.relay_device_id`); newer = greater `event_sort_key`. `DeviceAnnounce` and all membership kinds are NEVER superseded.
- Watermark thresholds: warn at ≥ 50% of `MAX_PAYLOAD_SIZE`, degraded report `state_root_near_cap` at ≥ 80%, degraded report `state_root_over_cap` on encode failure.
- Core branch: `zeb-813-verified-log-supersession` off harmony `main` (`4eb4208`). Client branch: `zeb-813-announce-supersession` (exists, carries spec).
- Commit trailer: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` + `Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc`.

---

### Task 1: Core supersession seam (harmony repo)

**Files:**
- Modify: `~/work/zeblithic/harmony/crates/harmony-crdt-sync/src/verified_log.rs` (trait at :50, `InsertOutcome` at :88, `insert` at :150, `from_verified_events` at :127, inline `mod tests` at :204)

**Interfaces:**
- Consumes: existing `LogPolicy` (associated fns `event_id`, `cmp`, `verify`, `materialize`), `VerifiedLog { events: BTreeMap<P::EventId, P::Event> }`.
- Produces: `LogPolicy::supersedes(newer: &Self::Event, older: &Self::Event) -> bool` (defaulted `false`); `InsertOutcome::Superseded` (unit variant, no payload); compaction behavior in `insert` + `from_verified_events`. Task 3 matches on `Superseded` and implements `supersedes`.

- [ ] **Step 1: branch + write failing tests** — `git checkout -b zeb-813-verified-log-supersession origin/main`. In the existing `mod tests`, add a supersession toy policy and five tests:

```rust
/// ZEB-813 toy: same `key`, greater order supersedes. `verify` counts
/// invocations through the context so tests can prove Superseded skips it.
struct SupToy;

#[derive(Clone)]
struct SupEvent {
    id: u32,
    order: u64,
    key: u32,
}

impl LogPolicy for SupToy {
    type Event = SupEvent;
    type EventId = u32;
    type State = BTreeSet<u32>;
    type Context = core::cell::Cell<u32>; // verify-invocation counter
    type Error = ();

    fn event_id(e: &SupEvent) -> u32 {
        e.id
    }
    fn cmp(a: &SupEvent, b: &SupEvent) -> Ordering {
        a.order.cmp(&b.order).then(a.id.cmp(&b.id))
    }
    fn verify(_e: &SupEvent, _prior: &BTreeSet<u32>, ctx: &Self::Context) -> Result<(), ()> {
        ctx.set(ctx.get() + 1);
        Ok(())
    }
    fn materialize(events: &[&SupEvent], _ctx: &Self::Context) -> BTreeSet<u32> {
        events.iter().map(|e| e.id).collect()
    }
    fn supersedes(newer: &SupEvent, older: &SupEvent) -> bool {
        newer.key == older.key && Self::cmp(newer, older) == Ordering::Greater
    }
}

fn sup(id: u32, order: u64, key: u32) -> SupEvent {
    SupEvent { id, order, key }
}

#[test]
fn supersession_converges_across_all_arrival_orders() {
    let evs = [sup(1, 10, 7), sup(2, 20, 7), sup(3, 30, 7)];
    let orders: [[usize; 3]; 6] = [
        [0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0],
    ];
    for order in orders {
        let ctx = core::cell::Cell::new(0);
        let mut log = VerifiedLog::<SupToy>::new();
        for i in order {
            log.insert(evs[i].clone(), &ctx);
        }
        let ids: Vec<u32> = log.events().map(|e| e.id).collect();
        assert_eq!(ids, vec![3], "arrival order {order:?} must retain only newest");
    }
}

#[test]
fn different_keys_never_supersede() {
    let ctx = core::cell::Cell::new(0);
    let mut log = VerifiedLog::<SupToy>::new();
    assert_eq!(log.insert(sup(1, 10, 7), &ctx), InsertOutcome::Inserted);
    assert_eq!(log.insert(sup(2, 20, 8), &ctx), InsertOutcome::Inserted);
    assert_eq!(log.len(), 2);
}

#[test]
fn superseded_on_arrival_skips_verify() {
    let ctx = core::cell::Cell::new(0);
    let mut log = VerifiedLog::<SupToy>::new();
    assert_eq!(log.insert(sup(2, 20, 7), &ctx), InsertOutcome::Inserted);
    assert_eq!(ctx.get(), 1);
    assert_eq!(log.insert(sup(1, 10, 7), &ctx), InsertOutcome::Superseded);
    assert_eq!(ctx.get(), 1, "Superseded must not run verify");
    assert_eq!(log.len(), 1);
}

#[test]
fn from_verified_events_compacts() {
    let log = VerifiedLog::<SupToy>::from_verified_events([
        sup(1, 10, 7),
        sup(2, 20, 7),
        sup(3, 30, 9),
    ]);
    let ids: Vec<u32> = log.events().map(|e| e.id).collect();
    assert_eq!(ids, vec![2, 3]);
}

#[test]
fn default_policy_never_supersedes() {
    // `Toy` has no `supersedes` impl — the default keeps every event and
    // never returns Superseded, byte-identical to pre-ZEB-813 behavior.
    let mut log = VerifiedLog::<Toy>::new();
    for id in [1u32, 2, 3] {
        let outcome = log.insert(
            ToyEvent { id, order: id as u64, needs_prior: false },
            &(),
        );
        assert_eq!(outcome, InsertOutcome::Inserted);
    }
    assert_eq!(log.len(), 3);
}
```

(`ToyEvent` field spelling: confirm against the existing `Toy` policy in the file and match it exactly.)

- [ ] **Step 2: run to verify failure** — `cd ~/work/zeblithic/harmony && cargo test -p harmony-crdt-sync verified_log`. Expected: compile FAIL — `supersedes` not on `LogPolicy`, no `Superseded` variant.

- [ ] **Step 3: implement** — three edits per spec §1:

(a) On `LogPolicy`, after `materialize`, the defaulted method with the four-clause contract doc (copy the doc comment verbatim from spec §1):

```rust
    fn supersedes(_newer: &Self::Event, _older: &Self::Event) -> bool {
        false
    }
```

(b) On `InsertOutcome`, after `AlreadyKnown`:

```rust
    /// The event was new but an already-stored event supersedes it; it was
    /// dropped WITHOUT running `verify` (mirrors `AlreadyKnown`'s
    /// verify-skip: a stale record changes nothing, so proving it valid
    /// buys nothing). Callers treat this exactly like `AlreadyKnown`.
    Superseded,
```

(c) In `insert`, after the `AlreadyKnown` early-return:

```rust
        if self.events.values().any(|existing| P::supersedes(existing, &event)) {
            return InsertOutcome::Superseded;
        }
```

and in the `Ok(())` arm, before `self.events.insert(id, event)`:

```rust
                let stale: Vec<P::EventId> = self
                    .events
                    .values()
                    .filter(|existing| P::supersedes(&event, existing))
                    .map(|existing| P::event_id(existing))
                    .collect();
                for stale_id in stale {
                    self.events.remove(&stale_id);
                }
```

(d) In `from_verified_events`, after the dedup loop:

```rust
        // ZEB-813: deterministic supersession compaction at load. "Not
        // superseded by any other present event" is the semilattice
        // maximum directly, so trusted restores land on the same set an
        // insert-ordered replica converges to — including already-over-cap
        // logs persisted by pre-supersession builds.
        let stale: Vec<P::EventId> = map
            .values()
            .filter(|older| map.values().any(|newer| P::supersedes(newer, older)))
            .map(|older| P::event_id(older))
            .collect();
        for stale_id in stale {
            map.remove(&stale_id);
        }
```

- [ ] **Step 4: run to verify pass** — `cargo test -p harmony-crdt-sync`. Expected: all pass, including pre-existing tests (default-policy behavior unchanged).

- [ ] **Step 5: crate gates + commit** — `cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings` (repo-standard flags; check `.github/workflows` if clippy flags differ). Commit:

```bash
git add crates/harmony-crdt-sync/src/verified_log.rs
git commit -m "crdt-sync: deterministic supersession seam on VerifiedLog (ZEB-813 PR 1/2)"
```

### Task 2: Core PR (harmony repo)

**Files:** none beyond Task 1.

- [ ] **Step 1:** `git push -u origin zeb-813-verified-log-supersession`
- [ ] **Step 2:** `gh pr create --repo zeblithic/harmony` — title `crdt-sync: deterministic supersession seam on VerifiedLog (ZEB-813 PR 1/2)`; body: mechanism summary, link ZEB-813, note the new `InsertOutcome::Superseded` variant is deliberately compile-breaking for downstream exhaustive matches; standard footer. Record the PR number and branch head SHA for Task 3.
- [ ] **Step 3:** Fire the review-bot trigger once per repo convention (check for an in-flight trigger first; ONE `@` at open, then zero).

### Task 3: Client policy + boundary mapping (harmony-client repo)

**Files:**
- Modify: `src-tauri/Cargo.toml` (harmony crate revs — ALL to Task 2's branch head SHA, lockstep)
- Modify: `src-tauri/src/community_state_crdt.rs` (`impl LogPolicy for MembershipPolicy` at :324; `insert_event` match at :483)
- Test: `src-tauri/src/community_membership.rs` (test module — has `make_reachability_event` :15626, `make_relay_announce_event` :15876) and `src-tauri/src/community_state_crdt.rs` tests

**Interfaces:**
- Consumes: core `supersedes` + `Superseded` (Task 1); `event_sort_key` (community_membership.rs); `MembershipEventKind::{ReachabilityAnnounce, CommunityRelayAnnounce}`; `ReachabilityAnnouncePayload.iroh_node_id: [u8; 32]`; `CommunityRelayAnnouncePayload.relay.relay_device_id: [u8; 16]`.
- Produces: `MembershipPolicy::supersedes`; `insert_event` mapping `CoreOutcome::Superseded => InsertOutcome::AlreadyKnown`.

- [ ] **Step 1: lockstep rev bump** — in `src-tauri/Cargo.toml` set every `harmony-*` git dependency's `rev` to Task 2's branch head SHA (one shared rev; `pkarr` keeps its own pin). `cargo check --locked` fails on the new variant — expected; add the mapping in the same step or accept red until Step 3.

- [ ] **Step 2: write failing tests.** In `community_membership.rs` tests (builders in scope):

```rust
// ── ZEB-813 supersession policy tests ──────────────────────────────

#[test]
fn zeb813_newer_same_actor_same_node_supersedes() {
    // Two announces, same member, same iroh_node_id, HLC w=1000 then w=2000.
    // Build via make_reachability_event; assert MembershipPolicy::supersedes
    // (newer, older) == true and (older, newer) == false.
}

#[test]
fn zeb813_different_actor_or_node_never_supersedes() {
    // Same-kind pairs across (different actor, same node) and
    // (same actor, different node): both directions false.
}

#[test]
fn zeb813_relay_announce_keys_on_relay_device_id() {
    // make_relay_announce_event pairs: same (actor, relay_device_id)
    // supersedes by sort key; different relay_device_id false.
    // Cross-kind (reachability vs relay) false in both directions.
}

#[test]
fn zeb813_membership_kinds_never_supersede() {
    // A Join/SetPower/DeviceAnnounce pair (reuse existing builders in this
    // module) never supersedes in any direction or combination with an
    // announce.
}

#[test]
fn zeb813_materialize_neutrality_full_vs_compacted() {
    // Contract (d): build a log of N members' membership events + M stale
    // announces per member; materialize(full) == materialize(latest-only).
    // Assert MaterializedMembership equality (derives PartialEq — verify;
    // if not, compare the fields the struct exposes).
}
```

Fill in real construction using the module's existing member/community fixtures (the RCH test block at :15584 shows the full recipe: community setup, member join, `make_reachability_event(community, &member, node_id, announced_at, wall_ms)` — reuse its patterns verbatim). In `community_state_crdt.rs` tests:

```rust
#[test]
fn zeb813_insert_event_maps_superseded_to_already_known() {
    // CommunityState::new + insert newer announce then older announce
    // (signed via the community_membership test builders — make them
    // pub(crate) under cfg(test) if cross-module visibility requires).
    // Assert second insert returns InsertOutcome::AlreadyKnown and
    // log length is 1.
}

#[test]
fn zeb813_from_verified_events_heals_stale_announce_pile() {
    // Feed from_verified_events K stale + 1 fresh announce per member +
    // the membership events; assert events() retains exactly
    // members + fresh announces, and materialized() equals the
    // pre-compaction materialization.
}
```

- [ ] **Step 3: implement.** In `impl LogPolicy for MembershipPolicy` add (exact code from spec §2 — the `same_key` match + `event_sort_key` comparison). In `insert_event`'s match add:

```rust
            CoreOutcome::Superseded => {
                // ZEB-813: a stale announce a stored event supersedes.
                // Nothing changed — no cache bump, no dirty mark, no
                // persist. AlreadyKnown is exactly那 contract, so callers
                // need no new arm.
                InsertOutcome::AlreadyKnown
            }
```

(Fix the stray non-ASCII in that comment when writing it — it must read "exactly that contract".)

- [ ] **Step 4: run** — `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(zeb813)'`. Expected: all zeb813 tests pass.
- [ ] **Step 5: commit** — `git add -A && git commit -m "community CRDT: stale announces supersede instead of accumulating (ZEB-813)"`.

### Task 4: Watermark surfacing (harmony-client repo)

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs` (`encode_root_packet` at :3067 — after the `encrypt_blob` call at :3144, and the `for_book` error arm at :3149-3156; check whether `report_degraded` + `ctx.error_tx` are reachable from `encode_root_packet`'s `&InternalCtx` — they are used elsewhere in the same file with `ctx.error_tx.as_ref()`)
- Test: same file's test module (or the engine-test module the existing degraded-report tests live in — grep `report_degraded` tests)

**Interfaces:**
- Consumes: `harmony_content::cid::MAX_PAYLOAD_SIZE`, existing `report_degraded(error_tx, community_id, class, detail)`.
- Produces: log/report behavior only — no new public API.

- [ ] **Step 1: write failing tests** — unit-test the threshold classifier, not the whole engine. Extract:

```rust
/// ZEB-813: size-watermark classification for the encoded root blob.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RootSizeWatermark {
    Ok,
    NearHalf,   // >= 50% of MAX_PAYLOAD_SIZE
    NearCap,    // >= 80%
}

pub(crate) fn classify_root_size(len: usize) -> RootSizeWatermark { … }
```

Tests: boundary values (49%/50%/79%/80%/100% of `MAX_PAYLOAD_SIZE`) map to `Ok`/`NearHalf`/`NearHalf`/`NearCap`/`NearCap`.

- [ ] **Step 2: implement + wire.** In `encode_root_packet` after `encrypt_blob`: match `classify_root_size(blob_ciphertext.len())` → `NearHalf` = `tracing::warn!` (community id, len, cap, percent); `NearCap` = warn + `report_degraded(ctx.error_tx.as_ref(), ctx.community_id, "state_root_near_cap", …)`. In the `for_book` `Err` map (:3156): additionally `report_degraded(…, "state_root_over_cap", …)` before returning the error. Serve + publish both route through `encode_root_packet`, so one site covers both paths (verify `publish_root_now` doesn't have a second `for_book` call — grep `for_book` in the file).

- [ ] **Step 3: run** — `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(watermark) or test(root_size)'`. Expected: pass.
- [ ] **Step 4: commit** — `"community sync: state-root size watermarks + over-cap degraded reports (ZEB-813)"`.

### Task 5: Gates, PRs, convergence

- [ ] **Step 1: client gates** — from `src-tauri/`: fmt check, clippy `--locked --all-targets --features test-fixtures --no-deps -- -D warnings`, then `scripts/test-select`; fix anything red. **This PR changes the dependency graph (Cargo.toml/Cargo.lock rev bump), so per the workspace rule test-select must run as `scripts/test-select --full`** — the script itself refuses `--context task` on dep-graph changes and execution confirmed that refusal; the full sweep of Step 2 is the gate. When a future round uses selective mode (docs/test-only fixes), paste the printed `round=… bucket=…` summary line into the task report so the selection is auditable.
- [ ] **Step 2: full sweep** — `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (background, wall-clock net). This satisfies Step 1's `--full` requirement. Frontend untouched — no tsc/vitest needed unless CI disagrees.
- [ ] **Step 3: core PR merge gate** — the client PR cannot point at an unmerged branch SHA long-term. When the core PR merges (Jake's gate), re-pin all harmony revs to the MERGE SHA, `cargo update -p harmony-crdt-sync --precise` as needed / regenerate `Cargo.lock`, re-run Step 1 gates, push.
- [ ] **Step 4: open client PR** — `gh pr create --repo zeblithic/harmony-client` from `zeb-813-announce-supersession`; body: mechanism, heal-on-load, watermark surfacing, spec + plan paths, `Fixes ZEB-813`; standard footer; ONE review-bot trigger.
- [ ] **Step 5: convergence loop** — both PRs: scan all three comment buckets, bundle fixes, ONE push per round, wait CI green + bots clean. Never merge.
- [ ] **Step 6: post-merge fleet validation** — rebuild + restart Koya nodes; success criteria: `payload too large` absent from fresh fleet-koya log, a successful root publish logged, `crdt.cbor` shrinks (~1.085 MB → ~60 KB) after first post-fix persist; post numbers to fleet board + ZEB-813; verify Linear auto-close.

## Self-Review (completed at write time)

- Spec coverage: §1→Task 1; §2→Task 3; §3→Task 4; §4 non-goals honored (no wire change, no migration tooling, ZEB-814/815 untouched); Testing section→Tasks 1/3/4; Rollout→Tasks 2/5. Client test 4 from the spec ("engine ingest of stale-announce blob") is covered by the `insert_event` mapping test (Task 3) — the engine path collapses to that boundary. Watermark spec test 5's "both paths" collapses to the single `encode_root_packet` site (verified single `for_book` call during Task 4 Step 2).
- Placeholders: policy-matrix tests carry construction recipes by named existing builders + the RCH test block as the verbatim pattern — deliberate reuse, not placeholders. One stray non-ASCII char in a Task 3 code comment is flagged for correction at write time.
- Type consistency: `RootSizeWatermark` names match between Task 4 steps; `Superseded` arm name matches Task 1's variant; `sup()` helper signature consistent across Task 1 tests.
