# ZEB-702 Fleet-Sibling Sync Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make community-less SAS-paired fleet siblings converge owner-state/FleetNetDoc over the existing owner-scoped sync fabric, so a cert-only butler's `friend_graph` populates and deposits are authorized (ZEB-702).

**Architecture:** Two product gaps, two seams: (A) the supervisor boot-seed enumerates a resolver view that drops fleet-slot-only peers — add an inclusive dial view; (B) sync engines never re-offer state on link-up — add a transport-epoch-subscribed republish nudge. Plus (D) local-only observability for butler-deposit rejects. Spec: `docs/specs/2026-07-16-zeb-702-fleet-sibling-sync-design.md` (read it first).

**Tech Stack:** Rust (src-tauri), tokio watch channels, existing `FleetSyncEngine` machinery.

## Global Constraints

- NO wire-format change anywhere. Wire-format fixture tests (`tests/wire_format_*_fixtures.rs`) must stay byte-identical.
- `durable_preferred()` (`reachability_resolver.rs:196`) must NOT change — existing `resolve()` / `list_active_peers()` callers keep durable/pkarr semantics.
- No butler-acceptor authorization change; the wire-silent reject (no oracle) stays wire-silent. New WARN/counters are local-only.
- e2e/s7 asserts untouched (do not weaken or harden the co-located HELD boundary).
- DTO fields serialize camelCase (serde) — e2e assertions read camelCase keys.
- Gates per task: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `scripts/test-select --context task` (from repo root; paste the printed `round=… bucket=…` summary line into the task report so the selection is auditable). Commit BEFORE gates; 10-minute wall-clock budget on gate runs; report DONE_WITH_CONCERNS rather than looping.
- Commits end with:
  `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` and
  `Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc`
- Timing-sensitive tests use tokio paused time or budgets far below regression thresholds.

---

### Task 1: Resolver dial view + boot-seed inclusion (Component A)

**Files:**
- Modify: `src-tauri/src/reachability_resolver.rs` (new method near `list_active_peers_with_source`, ~line 476)
- Modify: `src-tauri/src/iroh_zenoh_registration.rs` (`boot_seed_node_ids_by_recency`, above line 120)
- Tests: inline `#[cfg(test)]` in both files (existing test modules)

**Interfaces:**
- Produces: `ReachabilityResolver::list_dialable_peers(&self) -> Vec<(OwnerAddr, ResolverEntry)>` — freshest entry per `(owner, node_id)` key across ALL three slots (durable/pkarr/fleet), i.e. `slots.freshest()`, cloned. One row per (owner, device), like `list_active_peers`.

- [ ] **Step 1: Failing tests.** In `reachability_resolver.rs` tests: (a) insert a FleetSibling-only entry (`update_with_source(..., ReachabilitySource::FleetSibling)`) → `list_dialable_peers()` returns it while `list_active_peers()` does NOT (pins the deliberate asymmetry); (b) durable+fleet both present → the freshest wins (reuse existing freshest-precedence fixtures as models, e.g. the ZEB-510 tests near lines 2075-2176). In `iroh_zenoh_registration.rs` tests: fleet-only entry → its node-id appears in `boot_seed_node_ids_by_recency` output, recency-ordered by `effective_announced_at_ms`, self-node still excluded.
- [ ] **Step 2: Run tests, verify they fail** (`cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(list_dialable) or test(boot_seed)'`).
- [ ] **Step 3: Implement.** Add `list_dialable_peers` (mirror `list_active_peers_with_source`'s shape, but map via `v.freshest()` and return the full `ResolverEntry` clone). Switch `boot_seed_node_ids_by_recency`'s enumeration from its current `durable_preferred`-based source to `list_dialable_peers`, keeping the existing recency sort + self-exclusion exactly. Doc-comment WHY (ZEB-702: fleet-slot-only siblings must be re-dialed at boot; kick gate already used freshest — this aligns boot with runtime).
- [ ] **Step 4: Tests green.**
- [ ] **Step 5: Commit** (`ZEB-702 T1: boot-seed dials fleet-sibling-only peers (list_dialable_peers)`), then gates.

### Task 2: `RepublishDirty` seam on the sync engines (Component B, part 1)

**Files:**
- Modify: `src-tauri/src/fleet_sync.rs` (trait + impl near `notify_dirty`, ~line 250)
- Tests: inline in `fleet_sync.rs`

**Interfaces:**
- Produces: `pub trait RepublishDirty: Send + Sync { fn republish_dirty(&self); }` implemented for `FleetSyncEngine<S>` by delegating to `notify_dirty()`. Also implemented for `mint_sync`'s engine ONLY IF it shares `FleetSyncEngine` (check: `mint_sync.rs` — if it is a distinct engine type, add the same one-line impl there; notes likewise). Task 3 consumes `Arc<dyn RepublishDirty>`.

- [ ] **Step 1: Failing test.** With an engine built via the existing `new_for_test*` constructor and a large debounce, call `republish_dirty()` via the trait object → engine publishes on the debounce path exactly as a `notify_dirty()` would (assert via the existing test publisher channel; model on the existing notify_dirty debounce test).
- [ ] **Step 2: Verify fail** (trait doesn't exist yet → compile fail is the failure mode; `cargo nextest list --locked` catches it fast).
- [ ] **Step 3: Implement** trait + impls (delegation only, no behavior).
- [ ] **Step 4: Tests green.**
- [ ] **Step 5: Commit** (`ZEB-702 T2: RepublishDirty seam on sync engines`), gates.

### Task 3: Transport-epoch republish listener (Component B, part 2)

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (listener task near the epoch machinery; handles struct(s) that `start_node` passes in)
- Modify: `src-tauri/src/lib.rs` (collect `Arc<dyn RepublishDirty>` clones of ALL owner dataset engines — owner-state `:4868`, fleet-net `:5627`, dm-inbox `:5160`, dm-outhold, owner-trust `:5863`, fleet-keys, owner-quorum-req, community-device-intro `:5255-5350`, mint `:5040`, notes, relay-hold, relay-optin — into a `Vec` threaded through the event-loop handles; the relay pair are owner-scoped `ds/*` engines like the rest)
- Tests: inline in `event_loop.rs` (fake `RepublishDirty` impl counting calls)

**Interfaces:**
- Consumes: `RepublishDirty` (Task 2), `transport_epoch_tx: watch::Sender<u64>` (`event_loop.rs:1086`, bumped at `:3830`; subscribe precedent `:3169`).
- Produces: a spawned listener: `let mut rx = transport_epoch_tx.subscribe(); loop { rx.changed().await?; for e in &engines { e.republish_dirty(); } }` — plus the handles-struct field carrying `Vec<Arc<dyn RepublishDirty>>` from `start_node`.

**Additional requirement (T1 review ⚠️, controller-adjudicated):** the zenoh
transport-events listener's zid→node-id reverse cache (`event_loop.rs:~1428`)
is built from `list_active_peers()`, so a fleet-only sibling's transport drop
never fires the secondary `Dropped` reconnect kick / liveness down-edge (the
registry drop-watchers remain the primary). Switch that ONE enumeration to
`list_dialable_peers()` (map `entry.payload` where the payload is needed) so
fleet siblings get the same drop-resilience as community peers. Add/extend a
test if the surrounding module has a harness for the reverse-cache mapping;
otherwise document the switch in the task report and rely on T1's view tests
(the mapping is a straight enumeration swap).

- [ ] **Step 1: Failing test.** Extract the listener body into a testable `pub(crate) async fn run_epoch_republish(rx: watch::Receiver<u64>, engines: Vec<Arc<dyn RepublishDirty>>)`. Test with fake engines + a real watch channel: (a) one `send_modify` bump → every fake sees exactly 1 call; (b) no bump → 0 calls; (c) two rapid bumps → calls ≥1 and ≤2 per fake (watch coalescing is allowed — assert the bound, not an exact count); (d) sender dropped → task exits (no hang; use `tokio::time::timeout` with paused time).
- [ ] **Step 2: Verify fail.**
- [ ] **Step 3: Implement** fn + spawn site + `lib.rs` collection/threading. The listener must NOT fire on the initial subscribe value (use `rx.changed().await` semantics — it only wakes on post-subscribe changes; add a comment pinning this).
- [ ] **Step 4: Tests green.**
- [ ] **Step 5: Commit** (`ZEB-702 T3: republish owner datasets on transport up-edge`), gates.

### Task 4: Butler-deposit accept/reject counters + rate-limited WARN (Component D, part 1)

**Files:**
- Modify: `src-tauri/src/iroh_butler_acceptor.rs` (decision sites around the authorization check ~:235/:292 and the reject log `:1013`)
- Tests: inline in `iroh_butler_acceptor.rs`

**Interfaces:**
- Produces: `pub struct ButlerDepositStats { accepted: AtomicU64, rejected_unauthorized: AtomicU64, rejected_other: AtomicU64 }` with `Arc` sharing + snapshot getter `fn snapshot(&self) -> (u64, u64, u64)` (or small struct). Injected into the acceptor (fluent setter or ctor param, default fresh). Mirror the dial-outcome counters (`network_health.rs:247-254`).
- WARN: on `rejected_unauthorized` increments, a rate-limited `tracing::warn!` (at most one per 60 s; simple `AtomicU64` last-warn-ms gate — no new dependency) naming ZEB-702 and the remedy hint ("sender not in this butler's roster — if persistent, owner-state sync to this sibling is not converging").

- [ ] **Step 1: Failing tests.** (a) unauthorized deposit → `rejected_unauthorized` == 1, WARN emitted once; (b) second unauthorized inside the window → counter 2, no second WARN (inject a test clock or use paused time); (c) accepted deposit → `accepted` == 1. Reuse the file's existing acceptor test harness/mocks.
- [ ] **Step 2: Verify fail.**
- [ ] **Step 3: Implement.** Wire-behavior UNCHANGED (same close-without-detail).
- [ ] **Step 4: Tests green.**
- [ ] **Step 5: Commit** (`ZEB-702 T4: butler-deposit decision counters + rate-limited local WARN`), gates.

### Task 5: Surface counters in network_health_snapshot (Component D, part 2)

**Files:**
- Modify: `src-tauri/src/network_health.rs` (snapshot assembly + DTO)
- Modify: `src-tauri/src/lib.rs` (thread the `Arc<ButlerDepositStats>` from the acceptor install site `~:9723` to the snapshot builder)
- Tests: inline in `network_health.rs`

**Interfaces:**
- Consumes: `ButlerDepositStats` (Task 4).
- Produces: snapshot DTO gains `butlerDeposits: { accepted, rejectedUnauthorized, rejectedOther }` (serde camelCase via the DTO's existing rename-all; field absent/zeroed when the node has no acceptor installed — follow the DTO's existing optional-section convention).

- [ ] **Step 1: Failing test.** Build a snapshot with a stats Arc at known counts → serialized JSON contains `"butlerDeposits":{"accepted":1,"rejectedUnauthorized":2,...}` (assert on serde_json Value, exact camelCase keys).
- [ ] **Step 2: Verify fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Tests green.**
- [ ] **Step 5: Commit** (`ZEB-702 T5: butler-deposit counters in network_health_snapshot`), gates.

### Task 6: Docs + full sweep

**Files:**
- Modify: `docs/playbooks/gce-cross-wan-runbook.md` (D3 section: "Known-red until ZEB-702 lands" → note the fix landed on this branch/PR, D3 re-validation pending; keep the debug-log classification guidance)
- No code.

- [ ] **Step 1: Edit runbook wording** (one paragraph; keep history accurate — the 2026-07-17 measured-results entry stays as-is).
- [ ] **Step 2: Full gates:** `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures` (the full sweep, not test-select).
- [ ] **Step 3: Commit** (`ZEB-702 T6: runbook D3 status + full-sweep gate`).

---

## Self-review notes

- Spec §A/§B/§D each map to exactly one task pair; §C is a no-op by design.
- Type names consistent: `RepublishDirty` (T2→T3), `ButlerDepositStats` (T4→T5).
- Task 3's engine list must match what `start_node` actually constructs — implementer verifies each cited line anchor before wiring and reports drift rather than guessing.
