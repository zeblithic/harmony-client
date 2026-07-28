# ZEB-787 Boot-Eager Voting Reconcile Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** After a node restart, reads of persisted voting state (`voting_get_tier2_proposal`, the Tier-3 GET, and the voting list verbs) return the proposal instead of "not found" / `[]`, without waiting for a mutating voting IPC to lazily load the community first.

**Architecture:** A small infallible helper reconciles each joined community's persisted voting log into `NodeState.voting_logs` at boot, hooked into `start_node`'s existing voting boot-setup block (right before the periodic voting tick is spawned). Reconcile-only — the voting engine still spawns lazily via `ensure_voting_engine_for`, which is idempotent with a pre-populated log. Read verbs and the lazy path are untouched.

**Tech Stack:** Rust, Tokio, `cargo-nextest`. Client-only (`harmony-client/src-tauri`); no harmony crate rev changes, no new dependencies.

## Global Constraints

- All cargo commands run from `src-tauri/`. Always include `--locked`. Include `--features test-fixtures` for any `--all-targets` or integration run.
- Full gates (must pass before PR): `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.
- Iterative per-task gate: `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(<name>)'` for the specific test; `scripts/test-select --context task` for a broader iterative sweep.
- **Reconcile-only.** Do NOT spawn voting engines at boot, and do NOT modify the read verbs (`voting_get_tier2_proposal_impl`, `voting_get_tier3_poll_raw`, list verbs) or the lazy `ensure_voting_engine_for` / `ensure_engine` path.
- **Infallible at boot.** The helper returns `()`. A per-community failure is logged and skipped; boot must never fail on voting reconcile.
- Commit-message trailer for every commit:
  ```
  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc
  ```

## Already Landed (context — do NOT redo)

On branch `zeb-787-boot-eager-voting-reconcile` (off `origin/main` @ 22c25115):
- `cde43706` — unit test `reconcile_restores_tier2_threshold_reached_timing` in the `zeb718_voting_reconcile_tests` module of `src-tauri/src/lib.rs` (mutation-proven; pins that `reconcile_voting_from_state` restores a `ThresholdReached` Tier-2 poll). This is the mechanism-1 guard; it is the template for the Task 1 fixtures/patterns.
- `e0c94e3c` — the design spec `docs/superpowers/specs/2026-07-28-zeb-787-boot-eager-voting-reconcile-design.md`.

## Reference Facts (verified against this branch)

- `reconcile_voting_from_state(voting_logs: &VotingLogsMap, identity_dir: Option<&Path>, community_id: SpaceId, membership_resolver: &Arc<dyn MembershipSnapshotResolver>) -> Result<(), String>` lives in `src-tauri/src/lib.rs` at ~line 53077. It returns `Err` only when `load_voting_log` returns `Err` (a present-but-unreadable file); a missing file is `Ok` (no-op); it early-returns `Ok` when the community's log already has events (idempotent).
- `load_voting_log` returns `Err(PersistError::Io)` when `std::fs::read` fails with a non-`NotFound` error. Reading a **directory** at the `voting.cbor` path produces `EISDIR` → `Err` (whereas a decode/version/id-mismatch quarantines to empty-`Ok`). This is how Task 1 forces the error-isolation path.
- `NodeStateMembershipResolver { pub community_registry: Arc<CommunitySyncRegistry>, pub crdt_state: Arc<Mutex<OwnerState>> }` at `lib.rs` ~53034 implements `MembershipSnapshotResolver`.
- `CommunitySyncRegistry::spawned_community_ids(&self) -> Vec<SpaceId>` (async) at `community_state_sync.rs:6016` returns the ids of communities with spawned engines.
- In `start_node`, the voting boot-setup block is at `lib.rs` ~13717–13769: it locks `state` (the `&std::sync::Mutex<NodeState>`), checks `guard.generation == our_gen`, clones `guard.voting_logs` (a direct `Arc`, not `Option`), and spawns the voting tick. `guard.community_registry` and `guard.crdt_state` are `Option<Arc<…>>`. `crate::owner_commands::resolve_identity_dir().ok()` yields `Option<PathBuf>` (the same identity-dir source the tick uses). `state`, `our_gen` are in scope; this is `start_node`'s async body (`.await` is valid).
- Test module `zeb718_voting_reconcile_tests` (`lib.rs` ~53191) has `use super::*;` and these fixtures/helpers in scope: `tier1_poll_create(actor, wall)`, `tier2_poll_create(actor, wall)`, `tier3_poll_create(actor, wall)`, `StubResolver { snapshot }`, and imports `Eligibility, MemberAttrs, MembershipSnapshot, PollEventKindCode, SignedVotingEvent, Tier`, `MembershipSnapshotResolver, SnapshotResolverError, VotingLog`, `Hlc, OwnerAddr, SpaceId`, `HashMap`, `Arc`. `VotingLogsMap` is in scope via `super::*`.

---

### Task 1: Boot-eager reconcile helper + sweep/isolation test

**Files:**
- Modify: `src-tauri/src/lib.rs` — add the helper immediately after `reconcile_voting_from_state` (before the `#[cfg(test)] mod zeb718_voting_reconcile_tests` at ~53190).
- Test: `src-tauri/src/lib.rs` — add the sweep test inside `mod zeb718_voting_reconcile_tests`.

**Interfaces:**
- Consumes: `reconcile_voting_from_state` (existing), `VotingLogsMap`, `MembershipSnapshotResolver`, `SpaceId`.
- Produces (Task 2 depends on this exact signature):
  ```rust
  async fn reconcile_all_joined_communities_voting(
      voting_logs: &VotingLogsMap,
      identity_dir: Option<&std::path::Path>,
      community_ids: &[crate::owner_state_types::SpaceId],
      membership_resolver: &std::sync::Arc<
          dyn crate::community_voting_log::MembershipSnapshotResolver,
      >,
  )
  ```

- [ ] **Step 1: Write the failing test** (inside `mod zeb718_voting_reconcile_tests`, e.g. after `reconcile_restores_tier2_threshold_reached_timing`)

```rust
    #[tokio::test]
    async fn reconcile_all_joined_communities_voting_sweeps_and_isolates_failures() {
        // ZEB-787: the boot sweep must (a) reconcile every joined community's
        // persisted voting log into `voting_logs` and (b) isolate a per-
        // community failure so one unreadable file never blocks the others.
        use crate::community_voting_core::Lifecycle;
        let dir = tempfile::tempdir().unwrap();
        let cid_ok_tier2 = SpaceId([0x81; 16]);
        let cid_ok_tier1 = SpaceId([0x82; 16]);
        let cid_unreadable = SpaceId([0x83; 16]);
        let actor = OwnerAddr([0xee; 16]);

        let mut members = HashMap::new();
        members.insert(actor, MemberAttrs { power: 10, vouching_depth: 0 });
        let snapshot = MembershipSnapshot { members };

        // Community A: a Tier-2 proposal at ThresholdReached (the ZEB-787 case).
        {
            let mut log = VotingLog::new();
            let pid = log
                .apply_with_snapshot(
                    tier2_poll_create(actor, 1_000),
                    &cid_ok_tier2,
                    Some(snapshot.clone()),
                )
                .expect("apply tier2");
            {
                let st = log.polls.get_mut(&pid).expect("materialized");
                st.meta.lifecycle = Lifecycle::ThresholdReached;
                st.tier_state
                    .as_tier2_mut()
                    .expect("tier2 state")
                    .threshold_reached_at_ms = Some(1_700_000_000_000);
            }
            let path = crate::community_voting_persist::voting_path_for(dir.path(), &cid_ok_tier2);
            crate::community_voting_persist::save_voting_log(&path, &log, &cid_ok_tier2).unwrap();
        }
        // Community B: a plain Tier-1 poll.
        {
            let mut log = VotingLog::new();
            log.apply_with_snapshot(
                tier1_poll_create(actor, 1_000),
                &cid_ok_tier1,
                Some(snapshot.clone()),
            )
            .expect("apply tier1");
            let path = crate::community_voting_persist::voting_path_for(dir.path(), &cid_ok_tier1);
            crate::community_voting_persist::save_voting_log(&path, &log, &cid_ok_tier1).unwrap();
        }
        // Community C: a present-but-unreadable voting.cbor. A directory at the
        // file path makes std::fs::read return EISDIR (a non-NotFound io error),
        // so load_voting_log returns Err (NOT quarantine) and reconcile_voting_
        // from_state returns Err — exercising the helper's skip-and-continue.
        {
            let path = crate::community_voting_persist::voting_path_for(dir.path(), &cid_unreadable);
            std::fs::create_dir_all(&path).unwrap();
        }

        let resolver: Arc<dyn MembershipSnapshotResolver> = Arc::new(StubResolver { snapshot });
        let voting_logs: VotingLogsMap = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

        // Unreadable community FIRST — proves its Err does not abort the sweep
        // before the healthy communities are reconciled.
        reconcile_all_joined_communities_voting(
            &voting_logs,
            Some(dir.path()),
            &[cid_unreadable, cid_ok_tier2, cid_ok_tier1],
            &resolver,
        )
        .await;

        let map = voting_logs.lock().await;
        assert!(
            map.contains_key(&cid_ok_tier2),
            "Tier-2 community reconciled despite an earlier failure in the sweep"
        );
        assert!(map.contains_key(&cid_ok_tier1), "Tier-1 community reconciled");
        assert!(
            !map.contains_key(&cid_unreadable),
            "unreadable community skipped, not inserted"
        );
        assert_eq!(map.len(), 2, "exactly the two healthy communities are present");
        let g = map.get(&cid_ok_tier2).unwrap().lock().await;
        assert_eq!(g.polls.len(), 1, "Tier-2 poll rematerialized through the sweep");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(reconcile_all_joined_communities_voting_sweeps_and_isolates_failures)'`
Expected: FAIL — compile error, `cannot find function reconcile_all_joined_communities_voting`.

- [ ] **Step 3: Implement the helper** (in `lib.rs`, immediately after `reconcile_voting_from_state`'s closing `}` and before `#[cfg(test)]\nmod zeb718_voting_reconcile_tests {`)

```rust
/// ZEB-787: boot-eager voting restore. Loads each joined community's persisted
/// voting log into `voting_logs` so read verbs (`voting_get_tier2_proposal`,
/// the Tier-3 GET, list verbs) answer for persisted governance state
/// immediately after a restart, rather than returning "not found" / `[]` until
/// a mutating voting IPC lazily reconciles the community.
///
/// Reconcile-only: this does NOT spawn engines. The lazy
/// `ensure_voting_engine_for` path still owns engine/subscriber creation and is
/// idempotent with a pre-populated log (it early-returns when events are
/// already present, then attaches the engine to the reloaded log).
///
/// Infallible to the caller: a per-community failure (a present-but-unreadable
/// `voting.cbor`, which `reconcile_voting_from_state` surfaces as `Err` to
/// disarm persistence) is logged and skipped so one bad file never blocks the
/// other communities or boot. A missing file is already a no-op inside
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
        if let Err(e) =
            reconcile_voting_from_state(voting_logs, identity_dir, community_id, membership_resolver)
                .await
        {
            tracing::warn!(
                ?community_id,
                err = %e,
                "boot voting reconcile failed for community; skipping (reads will lazily \
                 reconcile on the first mutating voting IPC for it)"
            );
        }
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(reconcile_all_joined_communities_voting_sweeps_and_isolates_failures)'`
Expected: PASS.

- [ ] **Step 5: Verify fmt + clippy on the changed crate**

Run: `cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: clean (no diff, no warnings).

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
ZEB-787: add boot-eager voting reconcile helper + sweep/isolation test

reconcile_all_joined_communities_voting loads each joined community's
persisted voting log into voting_logs; infallible (logs + skips a per-
community failure). Test proves the sweep reconciles multiple communities
and isolates an unreadable voting.cbor (directory at path -> EISDIR -> Err)
placed first in the list.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc
EOF
)"
```

---

### Task 2: Wire the helper into `start_node`'s voting boot-setup

**Files:**
- Modify: `src-tauri/src/lib.rs` — insert a boot-reconcile block immediately before the periodic-voting-tick block (the `// ── ZEB-291 Phase 2 Task 20 — periodic voting tick` comment, currently ~line 13697).

**Interfaces:**
- Consumes: `reconcile_all_joined_communities_voting` (Task 1); `NodeStateMembershipResolver`; `CommunitySyncRegistry::spawned_community_ids`; `state` (`&Mutex<NodeState>`), `our_gen`, in-scope in `start_node`.
- Produces: nothing new (integration only).

This block has no unit-level test (it lives inside `start_node`, which cannot be constructed in a unit test). Its verification is: it compiles, clippy is clean, and by inspection it calls the helper over the joined-community list at boot. The helper and the reconcile function it drives are unit-tested (Task 1 + `cde43706`).

- [ ] **Step 1: Insert the boot-reconcile block**

Locate the voting-tick block that begins with the comment `// ── ZEB-291 Phase 2 Task 20 — periodic voting tick ─────────` (~line 13697). Immediately **before** that comment, insert:

```rust
            // ── ZEB-787: boot-eager voting reconcile ───────────────────
            // Load each joined community's persisted voting log into
            // `voting_logs` so read verbs (voting_get_tier2_proposal, the
            // Tier-3 GET, list verbs) answer for persisted governance state
            // immediately after a restart, instead of returning "not found"
            // until a mutating voting IPC lazily reconciles the community.
            // Reconcile-only — the voting engine still spawns lazily via
            // ensure_voting_engine_for, which is idempotent with a pre-
            // populated log. Runs before the tick below so the first
            // threshold/finalize/archive sweep sees restored polls.
            {
                let boot_handles = {
                    let guard = state.lock().map_err(|e| format!("lock error: {e}"))?;
                    if guard.generation == our_gen {
                        match (guard.community_registry.clone(), guard.crdt_state.clone()) {
                            (Some(community_registry), Some(crdt_state)) => Some((
                                std::sync::Arc::clone(&guard.voting_logs),
                                community_registry,
                                crdt_state,
                            )),
                            _ => None,
                        }
                    } else {
                        None
                    }
                };
                if let Some((voting_logs_boot, community_registry, crdt_state_boot)) = boot_handles {
                    let community_ids = community_registry.spawned_community_ids().await;
                    let identity_dir = crate::owner_commands::resolve_identity_dir().ok();
                    let resolver: std::sync::Arc<
                        dyn crate::community_voting_log::MembershipSnapshotResolver,
                    > = std::sync::Arc::new(NodeStateMembershipResolver {
                        community_registry,
                        crdt_state: crdt_state_boot,
                    });
                    reconcile_all_joined_communities_voting(
                        &voting_logs_boot,
                        identity_dir.as_deref(),
                        &community_ids,
                        &resolver,
                    )
                    .await;
                }
            }

```

Notes for the implementer:
- The guard is dropped (end of the inner block) BEFORE any `.await` — never hold `state`'s `std::sync::Mutex` across an await.
- If `guard.community_registry` / `guard.crdt_state` field access does not compile as `Option` (verify against `VotingEngineNodeHandles::extract`, which does `guard.community_registry.clone().ok_or(...)?`), match the real types; the intent is "skip if the owner isn't loaded."
- Confirm placement with a scoped build (below); do not eye-count braces.

- [ ] **Step 2: Scoped build to confirm scope + types**

Run: `cd src-tauri && cargo build --locked -p harmony-app`
Expected: compiles. If `state` / `our_gen` / field option-ness differ, adjust per the compiler and re-run.

- [ ] **Step 3: fmt + clippy**

Run: `cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: clean.

- [ ] **Step 4: Full-suite gate (CI parity)**

Run: `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
Expected: PASS (including both ZEB-787 tests). This is the final regression gate for the change.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
ZEB-787: reconcile persisted voting logs at boot in start_node

Hook reconcile_all_joined_communities_voting into start_node's voting
boot-setup (before the periodic tick), over spawned_community_ids(), so a
read after restart finds persisted governance state instead of "not found".
Reconcile-only; read verbs and the lazy ensure_voting_engine_for path
unchanged.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc
EOF
)"
```

---

## Plan Self-Review

**Spec coverage:**
- Root-cause fix (populate `voting_logs` at boot) → Task 2 wiring. ✓
- The helper (`reconcile_all_joined_communities_voting`, infallible, log+skip) → Task 1. ✓
- Placement after community-engine spawn, membership resolver from `community_registry`+`crdt_state`, community list source → Task 2 (uses `spawned_community_ids()`, which only lists communities whose engines spawned — equivalent to "after the spawn loop"). ✓
- Idempotent with lazy path (reconcile early-returns on non-empty events) → relied on; no change needed to the lazy path. ✓
- Testing: keep the reconcile test (already landed) + add sweep/error-isolation test (Task 1); skip the e2e (Global Constraints / spec). ✓
- Read verbs untouched; reconcile-only; client-only → Global Constraints. ✓

**Placeholder scan:** No TBD/TODO. All steps carry runnable commands or complete code. The one judgement call (Option-ness of `community_registry`/`crdt_state`) is pinned to a concrete reference (`extract`) with a compiler-confirmed fallback. ✓

**Type consistency:** `reconcile_all_joined_communities_voting` signature is identical in Task 1 (definition) and Task 2 (call): `&VotingLogsMap`, `Option<&Path>` (`identity_dir.as_deref()`), `&[SpaceId]` (`&community_ids` from `Vec<SpaceId>`), `&Arc<dyn MembershipSnapshotResolver>` (`&resolver`). `NodeStateMembershipResolver` fields `community_registry`/`crdt_state` match `lib.rs:53034`. `spawned_community_ids()` returns `Vec<SpaceId>`. ✓
