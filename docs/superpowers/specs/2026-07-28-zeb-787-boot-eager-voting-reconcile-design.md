# ZEB-787: Boot-Eager Voting Reconcile — Design

**Ticket:** [ZEB-787](https://linear.app/zeblith/issue/ZEB-787) — "Tier-2 proposal survives restart on disk but not in memory — `voting_get_tier2_proposal` returns 'not found' for an id present in `voting.cbor`." Priority High.

**Goal:** After a node restart, a read of persisted governance state (`voting_get_tier2_proposal`, the Tier-3 GET, and the voting list verbs) returns the proposal instead of "not found" / `[]`, without waiting for a local *mutating* voting IPC to happen first.

## Root Cause (verified against `origin/main` @ 22c25115)

The ticket named two candidate mechanisms and declined to pick. A code trace + a mutation-proven unit test discriminate it as **mechanism 2** ("queried against an engine that was never populated for this community"):

- The reconcile machinery is gated entirely behind *mutating* voting IPCs. The call graph is a single chain with no other entrants:
  - `reconcile_voting_from_state` ← only `ensure_voting_engine_for` (`lib.rs`, the sole non-test caller)
  - `ensure_voting_engine_for` ← only `VotingEngineNodeHandles::ensure_engine`
  - `ensure_engine` ← only the 11 mutating `voting_*` IPC handlers (create / signal / delegate / cast / approve / …)
- **No boot path** eagerly reconciles joined communities. Grep of `reconcile_voting_from_state` and `ensure_voting_engine_for` across `src/` shows zero non-test, non-IPC callers.
- The **read verbs** (`voting_get_tier2_proposal_impl`, `voting_get_tier3_poll_raw`, and the list verbs — the `[]`/`[]` the ticket observed) read `NodeState.voting_logs` **directly** and call nothing that reconciles. `voting_get_tier2_proposal` takes only `proposal_id` (no community), scans all logs, and so cannot self-heal by ensuring a specific community's engine.

Therefore, after a restart `voting_logs` is empty for a community until some mutating voting IPC touches it. A cold read in that window returns "not found" for correctly-persisted state — exactly "survives on disk, not in memory."

**Mechanism 1 (the restore path) is NOT the bug.** `reconcile_voting_from_state` (ZEB-718) already restores a `ThresholdReached` Tier-2 proposal correctly: it replays the persisted `PollCreate` (re-materializing the poll into `.polls`) and applies the `poll_restore` overlay, whose `Tier2` arm restores `threshold_reached_at_ms` / `last_unsignal_after_threshold_ms`. This is mutation-proven (see Testing). The restore logic was sound; it was simply never invoked on the read path.

## Architecture

One small new helper plus one call site inside `start_node`'s existing per-community boot loop. **Read verbs and the lazy `ensure_engine` path are untouched.**

The chosen strategy is **reconcile-only** (approved 2026-07-28): boot loads each community's persisted voting log into `voting_logs` so reads succeed. The voting *engine* (subscriber + backfill) continues to spawn lazily on the first mutating voting IPC, exactly as today. This is the minimal change that resolves the ticket's read symptom and stays idempotent with the lazy path.

> **Explicitly out of scope (YAGNI):** reviving *live* voting delivery after restart (inbound peer voting events applying without a local action). That would require eager `ensure_voting_engine_for` per community at boot (many engine params, N subscriptions + backfills). The ticket concerns a single node reading its own persisted state; live-delivery-after-restart is a separate concern and, if wanted, a separate ticket.

## Components

### New helper: `reconcile_all_joined_communities_voting`

Location: `src-tauri/src/lib.rs`, adjacent to `reconcile_voting_from_state`.

```rust
/// Boot-eager voting restore (ZEB-787). Loads each joined community's
/// persisted voting log into `voting_logs` so read verbs
/// (`voting_get_tier2_proposal`, the Tier-3 GET, list verbs) answer for
/// persisted governance state immediately after a restart, rather than
/// returning "not found" until a mutating voting IPC lazily reconciles the
/// community. Reconcile-only: does NOT spawn engines — the lazy
/// `ensure_voting_engine_for` path still owns engine/subscriber creation and
/// is idempotent with a pre-populated log (it skips reload when events are
/// already present).
///
/// Infallible to the caller: a per-community failure (a present-but-unreadable
/// `voting.cbor`, which `reconcile_voting_from_state` surfaces as `Err` to
/// disarm persistence) is logged and skipped so one bad file never blocks the
/// other communities or boot itself. A missing file is already a no-op inside
/// `reconcile_voting_from_state`.
async fn reconcile_all_joined_communities_voting(
    voting_logs: &VotingLogsMap,
    identity_dir: Option<&std::path::Path>,
    community_ids: &[crate::owner_state_types::SpaceId],
    membership_resolver: &std::sync::Arc<
        dyn crate::community_voting_log::MembershipSnapshotResolver,
    >,
) {
    for &community_id in community_ids {
        if let Err(e) = reconcile_voting_from_state(
            voting_logs,
            identity_dir,
            community_id,
            membership_resolver,
        )
        .await
        {
            tracing::warn!(
                ?community_id,
                err = %e,
                "boot voting reconcile failed for community; skipping (read verbs will \
                 lazily reconcile on the first mutating voting IPC)"
            );
        }
    }
}
```

The helper's single responsibility is "load all persisted voting logs into memory at boot." Extracting it (rather than inlining a loop in `start_node`'s already-large boot block) gives an isolated, directly unit-testable seam for the sweep + error-isolation behavior; the per-community restore is already tested via `reconcile_voting_from_state`.

### Call site in `start_node`

Inside `start_node`, the existing boot block already snapshots joined communities into `community_snapshots` (kind == Community, `left_at.is_none()`, with `current_epoch_key` + `admin_addr`) and iterates them to spawn a community engine and per-channel channel-log engines.

The new call runs **after that spawn loop completes**, so every community's engine — the roster source a `NodeStateMembershipResolver` may consult during replay — is already up:

```rust
// After the `for (space_id, mk, admin, is_invite_only) in community_snapshots` loop:
// ZEB-787: boot-eager voting reconcile so reads answer for persisted governance
// state immediately after restart (engines still spawn lazily).
let voting_membership_resolver: std::sync::Arc<
    dyn crate::community_voting_log::MembershipSnapshotResolver,
> = std::sync::Arc::new(NodeStateMembershipResolver {
    community_registry: registry.clone(),
    crdt_state: crdt_state.clone(),
});
reconcile_all_joined_communities_voting(
    &voting_logs,
    identity_dir.as_deref(),
    &joined_community_ids,
    &voting_membership_resolver,
)
.await;
```

`joined_community_ids: Vec<SpaceId>` is the `space_id`s from `community_snapshots` (a community that failed to spawn its engine and hit the loop's `continue` is excluded — no engine means membership can't resolve, so its voting reconcile would no-op anyway).

Implementation note to verify during planning: `community_snapshots` is consumed by-value in the existing `for` loop, so the id list must be collected before/while iterating (e.g. `community_snapshots.iter().map(|(id, ..)| *id).collect()` before the loop). Confirm `identity_dir` and `NodeStateMembershipResolver` are in scope at this point (both are used by the surrounding boot code and by `ensure_engine`, respectively).

## Data Flow

1. **Boot:** `NodeState.voting_logs` starts empty.
2. Existing loop spawns community + channel-log engines per joined community.
3. **New:** `reconcile_all_joined_communities_voting` iterates the same community ids; each `reconcile_voting_from_state` loads `voting.cbor`, replays events (re-materializing polls into `.polls`), and applies the `poll_restore` overlay. `voting_logs[community]` is now populated.
4. **A read verb after boot** scans `voting_logs`, finds the poll, and answers — no "not found."
5. **A later mutating voting IPC** calls `ensure_engine` → `ensure_voting_engine_for` → `reconcile_voting_from_state`, which sees non-empty `events` and returns early (idempotent), then spawns the engine attached to the already-restored log. No double-load.

## Error Handling

- **Missing `voting.cbor`** (community never voted): `reconcile_voting_from_state` returns `Ok` without inserting — no-op. No pre-filtering needed.
- **Present-but-unreadable `voting.cbor`**: `reconcile_voting_from_state` returns `Err` (its signal to leave persistence disarmed). The helper logs and continues; that community's reads behave exactly as today (lazily reconciled on the first mutating IPC, which will hit the same `Err` and disarm). One bad file never blocks boot or the other communities.
- **Boot never fails on voting:** the helper returns `()`; it cannot abort `start_node`.
- **Un-replayable individual events** are already handled inside `reconcile_voting_from_state` (logged + skipped); unchanged.

## Testing

1. **Keep** `reconcile_restores_tier2_threshold_reached_timing` (already written on this branch, mutation-proven). It persists a `ThresholdReached` Tier-2 poll, reconciles from an empty registry, and asserts the poll is present, tier `Conviction`, lifecycle `ThresholdReached`, and both timing fields restored. Mutation evidence: deleting the `Tier2` overlay arm makes it fail with `threshold_reached_at_ms: None`; restoring passes. This pins mechanism-1 correctness.
2. **New helper test** `reconcile_all_joined_communities_voting_sweeps_and_isolates_failures` (in the `zeb718_voting_reconcile_tests` module): persist `voting.cbor` for two communities (one Tier-2-at-`ThresholdReached`, one Tier-1) plus one community whose `voting.cbor` path is a present-but-unreadable file; call the helper with an empty `voting_logs` and the stub resolver over all three ids; assert both good communities' polls are queryable in `voting_logs` and the unreadable one is skipped without affecting the others. Pins the boot-sweep + error-isolation contract.
3. **Deliberately NOT built:** a full conviction-to-threshold e2e restart-and-read test. It is disproportionately heavy for the current quota, and tests (1) + (2) already prove both the per-community restore and the multi-community sweep. The only link not directly unit-tested is the one wiring line "`start_node` calls the helper," verified by inspection + compile. (Approved 2026-07-28.)

Full-suite gates before PR: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, and the CI-parity nextest sweep (or `scripts/test-select` during iterative dev, full run for the final gate).

## Scope & Constraints

- **One PR** on branch `zeb-787-boot-eager-voting-reconcile` (off `origin/main` @ 22c25115), bundling: the reconcile unit test (already present), the new helper + its wiring, and the helper test.
- **No changes** to the read verbs or the lazy `ensure_engine` path.
- **No new dependencies.** Uses existing `reconcile_voting_from_state`, `NodeStateMembershipResolver`, `VotingLogsMap`, and the boot loop's in-scope `registry` / `crdt_state` / `identity_dir`.
- Harmony dep revs unchanged; this is client-only.
