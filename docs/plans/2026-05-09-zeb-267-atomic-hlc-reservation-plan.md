# ZEB-267 Atomic HLC Reservation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the snapshot-then-release HLC pattern in nine reservation sites across eight power-gated community-event IPCs with a single atomic `reserve_next_hlc_for_device` helper, closing the per-device monotone-HLC race surfaced by CodeRabbit on PR #93.

**Architecture:** Add a `reserve_next_hlc_for_device` async free function in `src-tauri/src/dm_outbox.rs` next to the existing `next_hlc`. Simplify the eight `mint_*` membership-event helpers in `src-tauri/src/lib.rs` to take a pre-reserved `Hlc` directly (drop their `wall_now_ms` / `device_id` / `prev_hlc` parameter trio). At each IPC site, call the helper once before mint and remove the post-Inserted tracker-advance block.

**Tech Stack:** Rust 1.x (workspace edition), Tokio 1.x async, ed25519-dalek signing, harmony-content CAS, Tauri 2.x IPC. Existing `next_hlc` helper at `src-tauri/src/dm_outbox.rs:1523` is unchanged.

---

## Conventions (apply to every task in this plan)

- **All `cargo` commands run from `src-tauri/`** (Cargo.toml lives there, not the repo root). Use `cd src-tauri && cargo ...` in shell steps; subagents working from the repo root must `cd src-tauri` first.
- **Pipe exit codes lie:** never trust `cmd | tail/grep` exit codes. Use `set -o pipefail` or `${PIPESTATUS[0]}`. Especially load-bearing when capturing test output.
- **No worktrees:** work happens directly in the main repo on branch `zeb-267-atomic-hlc-reservation` (already cut, spec committed at `70d7a99`).
- **Every task ends with a commit.** Verification gates (fmt + clippy + test) run BEFORE the commit, not after.
- **DO NOT use Monitor for `cargo test`** — wait synchronously.

---

## File Structure

| Path                                                     | Role                                | Touch type   |
| -------------------------------------------------------- | ----------------------------------- | ------------ |
| `src-tauri/src/dm_outbox.rs`                             | New `reserve_next_hlc_for_device` helper + 3 unit tests | Modify (Task 1) |
| `src-tauri/src/lib.rs`                                   | 8 `mint_*` helpers + 9 IPC reservation sites + 8 mint unit tests | Modify (Tasks 2 + 3) |
| `src-tauri/tests/community_hlc_race_integration.rs`      | New 2-task concurrent-mint test     | Create (Task 4) |
| `docs/specs/2026-05-09-zeb-267-atomic-hlc-reservation-design.md` | Design spec                  | Existing (committed at `70d7a99`) |
| `docs/plans/2026-05-09-zeb-267-atomic-hlc-reservation-plan.md`   | This plan                    | Created (Task 0 commit)   |

`src-tauri/src/dm_outbox.rs` already houses the existing `next_hlc` helper that the new reservation primitive wraps. `src-tauri/src/lib.rs` is the existing IPC layer; we don't introduce a new module — that would conflict with the spec §3.1 decision to keep the helper colocated with `next_hlc`.

---

## Task 0: Pre-flight + green baseline

**Goal:** Establish a green-from-clean-main baseline before any code changes. The spec and plan are already committed on the branch; Task 0 is purely a verification step.

**Files:**
- Read-only: `src-tauri/src/dm_outbox.rs`, `src-tauri/src/lib.rs`, `docs/specs/2026-05-09-zeb-267-atomic-hlc-reservation-design.md`, `docs/plans/2026-05-09-zeb-267-atomic-hlc-reservation-plan.md`

- [ ] **Step 1: Confirm working tree is clean and on the correct branch**

Run: `git status --short && git branch --show-current`
Expected: empty output for status; `zeb-267-atomic-hlc-reservation` for branch.

If working tree has untracked files or you're on a different branch, stop and reconcile with the user before proceeding.

- [ ] **Step 2: Confirm the spec and plan are committed**

Run: `git log --oneline -4`
Expected: the most recent two commits on this branch are the plan commit (`docs(zeb-267): implementation plan ...`) and the spec commit (`docs(zeb-267): atomic HLC reservation design spec` at `70d7a99`), with `b67468f` (the merge commit for PR #93) the next-most-recent.

- [ ] **Step 3: Verify cargo fmt baseline is green**

Run: `cd src-tauri && cargo fmt --all -- --check`
Expected: exit code 0, no output. If any file is unformatted, the baseline is broken — this is the test-drift-is-our-fault rule from the user memory; fix the drift in a preceding commit before continuing the implementation.

- [ ] **Step 4: Verify cargo clippy baseline is green**

Run: `cd src-tauri && cargo clippy --all-targets -- -D warnings`
Expected: exit code 0. Warnings are denied so any warning fails the build. If clippy emits warnings, they are pre-existing drift — fix them first (or file a Linear ticket per the unrelated-test-failures rule and merge that fix before continuing).

- [ ] **Step 5: Verify cargo test baseline is green**

Run: `cd src-tauri && cargo test --workspace --no-fail-fast 2>&1 | tee /tmp/zeb267-baseline.log; echo "test exit: ${PIPESTATUS[0]}"`
Expected: final line `test exit: 0`. Capture the test count from the log (e.g., `842 passed; 0 failed; 2 ignored`) — Task 5 verifies the count is unchanged at the end of the refactor (any new tests we add are accounted for in Tasks 1, 2, 4).

Task 0 produces NO commit. Verification only.

---

## Task 1: `reserve_next_hlc_for_device` helper + 3 unit tests

**Goal:** Add the reservation primitive in isolation. Helper-only commit. Helper is unused by the rest of the codebase at this point — Task 1 ships green entirely on its own merits.

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs:1515-1536` (add helper after the existing `next_hlc` at line 1523) and `src-tauri/src/dm_outbox.rs::tests` (add 3 tests inside the existing `mod tests` at line 1609)

- [ ] **Step 1: Read the existing `next_hlc` body and surrounding comment to locate the insertion point**

Read: `src-tauri/src/dm_outbox.rs:1515-1540`

This is the section directly after `next_hlc`. The existing comment at lines 1516-1522 says "A future cleanup could promote this to a shared module — out of Phase 2 scope." The new helper goes immediately after `next_hlc` — they're conceptual neighbors and both will eventually move together if the shared-module promotion ever happens.

- [ ] **Step 2: Write the failing helper unit tests FIRST (TDD discipline)**

The tests reference `reserve_next_hlc_for_device` which doesn't exist yet — `cargo test` will fail to compile, which is the expected red state.

Find the existing `mod tests` block at `src-tauri/src/dm_outbox.rs:1609`. Add these three tests inside the `mod tests { ... }` block, near the end (just before its closing `}`):

```rust
    // ── ZEB-267: reserve_next_hlc_for_device tests ─────────────────────
    //
    // Helper is the atomic read-bump-write primitive that replaces the
    // snapshot-then-release pattern at every membership-event IPC site.
    // These tests pin its three load-bearing properties:
    //
    //   1. Sequential reservations advance monotonically (sanity check).
    //   2. Concurrent reservations on the same tracker produce N distinct
    //      strictly-monotone HLCs (the actual bug fix — old pattern would
    //      collide here).
    //   3. Wall-clock regression (wall_now_ms < prev.wall_ms) still
    //      produces a strictly-greater HLC by clamping to prev.wall_ms +
    //      bumping logical (preserves monotonicity under clock skew).

    #[tokio::test]
    async fn reserve_next_hlc_for_device_advances_tracker_atomically() {
        use std::collections::BTreeMap;
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let tracker: Arc<Mutex<BTreeMap<String, Hlc>>> = Arc::new(Mutex::new(BTreeMap::new()));
        let device_id = "test-dev-A";
        let wall_now_ms = 1_700_000_000_000u64;

        let first = reserve_next_hlc_for_device(&tracker, device_id, wall_now_ms).await;
        let second = reserve_next_hlc_for_device(&tracker, device_id, wall_now_ms).await;

        // Sort key is (wall_ms, logical, device_id) — strictly-greater
        // ordering is what the receive side expects for per-device events.
        assert!(
            (second.wall_ms, &second.logical, &second.device_id)
                > (first.wall_ms, &first.logical, &first.device_id),
            "second reservation must be strictly greater than first under sort key"
        );
        // Tracker must hold the SECOND (just-bumped) value, not the first.
        let stored = tracker.lock().await.get(device_id).cloned().expect("tracker has entry");
        assert_eq!(
            stored, second,
            "tracker must hold the most-recently-reserved HLC"
        );
    }

    #[tokio::test]
    async fn reserve_next_hlc_for_device_concurrent_reservations_distinct() {
        use std::collections::{BTreeMap, BTreeSet};
        use std::sync::Arc;
        use tokio::sync::Mutex;
        use tokio::task::JoinSet;

        let tracker: Arc<Mutex<BTreeMap<String, Hlc>>> = Arc::new(Mutex::new(BTreeMap::new()));
        let device_id = "test-dev-conc";
        let wall_now_ms = 1_700_000_111_222u64;

        // Spawn 64 concurrent reservations. Without the atomic helper,
        // the snapshot-then-release pattern would produce duplicate
        // (wall_ms, logical, device_id) tuples across these tasks.
        let mut set: JoinSet<Hlc> = JoinSet::new();
        for _ in 0..64 {
            let tracker = Arc::clone(&tracker);
            let device_id = device_id.to_string();
            set.spawn(async move {
                reserve_next_hlc_for_device(&tracker, &device_id, wall_now_ms).await
            });
        }

        let mut hlcs: Vec<Hlc> = Vec::with_capacity(64);
        while let Some(joined) = set.join_next().await {
            hlcs.push(joined.expect("task panic"));
        }

        // Use sort-key tuples as the dedupe key (Hlc itself is Eq, but
        // BTreeSet<(u64, u32, String)> makes the failure message clearer
        // by surfacing the colliding tuple directly).
        let unique: BTreeSet<(u64, u32, String)> = hlcs
            .iter()
            .map(|h| (h.wall_ms, h.logical, h.device_id.clone()))
            .collect();
        assert_eq!(
            unique.len(),
            64,
            "all 64 concurrent reservations must yield distinct sort keys; got {} unique out of 64",
            unique.len()
        );

        // Tracker's final value must equal the max-by-sort-key of all
        // reservations (last-write-wins under the helper's atomic
        // critical section).
        let max_observed = hlcs
            .iter()
            .max_by_key(|h| (h.wall_ms, h.logical, h.device_id.clone()))
            .expect("at least one reservation");
        let stored = tracker.lock().await.get(device_id).cloned().expect("tracker has entry");
        assert_eq!(
            &stored, max_observed,
            "tracker's final value must equal the max-by-sort-key reservation"
        );
    }

    #[tokio::test]
    async fn reserve_next_hlc_for_device_handles_wall_regression() {
        use std::collections::BTreeMap;
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let tracker: Arc<Mutex<BTreeMap<String, Hlc>>> = Arc::new(Mutex::new(BTreeMap::new()));
        let device_id = "test-dev-regress";

        // Pre-seed the tracker with an HLC at wall_ms=1000, logical=5.
        {
            let mut t = tracker.lock().await;
            t.insert(
                device_id.to_string(),
                Hlc {
                    wall_ms: 1000,
                    logical: 5,
                    device_id: device_id.to_string(),
                },
            );
        }

        // Reserve with wall_now_ms=500 — strictly less than the prior
        // wall_ms. next_hlc clamps to prev.wall_ms and bumps logical.
        let reserved = reserve_next_hlc_for_device(&tracker, device_id, 500).await;
        assert_eq!(reserved.wall_ms, 1000, "wall_ms must clamp to prev.wall_ms under regression");
        assert_eq!(reserved.logical, 6, "logical must bump prev.logical + 1");
        assert_eq!(reserved.device_id, device_id);

        // Tracker must hold the new value.
        let stored = tracker.lock().await.get(device_id).cloned().expect("tracker has entry");
        assert_eq!(stored, reserved);
    }
```

- [ ] **Step 3: Run the new tests to verify they fail to compile**

Run: `cd src-tauri && cargo test --lib dm_outbox::tests::reserve_next_hlc_for_device 2>&1 | tee /tmp/zeb267-task1-red.log; echo "test exit: ${PIPESTATUS[0]}"`
Expected: non-zero exit code; build error mentioning `cannot find function reserve_next_hlc_for_device in this scope` (or similar). This is the red state we want before implementing.

- [ ] **Step 4: Add the helper after `next_hlc`**

Insert this code in `src-tauri/src/dm_outbox.rs` IMMEDIATELY AFTER the closing `}` of `next_hlc` (i.e., after line 1536). Match the surrounding style — the file uses tabs/spaces per the existing rustfmt config; do not reformat surrounding code.

```rust
/// Atomically reserve the next HLC for a device.
///
/// Acquires `tracker`, reads the device's last-known HLC, computes
/// the successor via `next_hlc`, writes it back, and returns it —
/// all under a single lock acquisition. Replaces the
/// snapshot-then-release pattern at all power-gated community-event
/// IPCs (kick / leave / set_power / channel_* / redeem /
/// create_community).
///
/// Tracker is bumped at reservation time, regardless of whether the
/// caller's downstream `engine.insert_local_event` succeeds. A
/// rejected insert "burns" the reserved HLC — fine, since HLCs are
/// 64-bit logical and burning is already implicit on signature- or
/// verify-failure paths today.
///
/// ZEB-267 — replaces the snapshot-then-release pattern that had a
/// race window between the `prev_hlc` read and the post-`Inserted`
/// advance. See `docs/specs/2026-05-09-zeb-267-atomic-hlc-reservation-design.md`.
pub async fn reserve_next_hlc_for_device(
    tracker: &std::sync::Arc<
        tokio::sync::Mutex<std::collections::BTreeMap<String, Hlc>>,
    >,
    device_id: &str,
    wall_now_ms: u64,
) -> Hlc {
    let mut t = tracker.lock().await;
    let prev = t.get(device_id).cloned();
    let next = next_hlc(prev.as_ref(), wall_now_ms, device_id);
    t.insert(device_id.to_string(), next.clone());
    next
}
```

- [ ] **Step 5: Run the three new tests to verify they pass**

Run: `cd src-tauri && cargo test --lib dm_outbox::tests::reserve_next_hlc_for_device 2>&1 | tee /tmp/zeb267-task1-green.log; echo "test exit: ${PIPESTATUS[0]}"`
Expected: final line `test exit: 0`; output contains `running 3 tests` and `3 passed`.

- [ ] **Step 6: Run the full workspace test to confirm no regression elsewhere**

Run: `cd src-tauri && cargo test --workspace --no-fail-fast 2>&1 | tee /tmp/zeb267-task1-full.log; echo "test exit: ${PIPESTATUS[0]}"`
Expected: final line `test exit: 0`; the test count is the Task 0 baseline + 3 (the three new tests).

- [ ] **Step 7: Run fmt and clippy**

Run: `cd src-tauri && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings`
Expected: both exit 0. If fmt finds drift on the new helper, run `cargo fmt --all` and stage the formatting fix into the same commit (don't commit unformatted code).

- [ ] **Step 8: Commit**

Run:
```bash
git add src-tauri/src/dm_outbox.rs
git commit -m "$(cat <<'EOF'
feat(zeb-267): atomic reserve_next_hlc_for_device helper

New async free function in dm_outbox.rs alongside the existing next_hlc.
Holds the tracker lock for one read-bump-write critical section, returns
the just-reserved Hlc. Replaces the snapshot-then-release pattern that
had a race window between the prev_hlc read and the post-Inserted
advance — see PR #93 CodeRabbit thread.

Three unit tests cover the load-bearing properties: sequential
monotonicity (sanity), 64 concurrent reservations all distinct (the
actual bug fix), and wall-clock regression (logical bump preserves
monotonicity under clock skew).

Helper is unused by callers at this commit; Tasks 2-3 wire it into
the eight membership-event mint helpers and nine IPC reservation
sites in lib.rs.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 9: Verify the commit landed**

Run: `git log --oneline -2`
Expected: HEAD is the just-created Task 1 commit; the previous commit is the Task 0 plan commit.

---

## Task 2: Mint helper signature change (8 helpers + 8 unit tests)

**Goal:** Replace the `(wall_now_ms: u64, device_id: &str, prev_hlc: Option<&Hlc>)` parameter trio with a single `hlc: Hlc` on each `mint_*` membership-event helper. Drop the internal `next_hlc(...)` call. Update each helper's local unit test to construct an explicit `Hlc` and pass it.

**IMPORTANT — INTENTIONAL BREAKAGE:** at the end of this task, the eight IPC call sites in `lib.rs` will NOT compile. That is expected — Task 3 fixes them all. Do NOT attempt to make the workspace compile by patching IPC sites in this task.

**Files:**
- Modify: `src-tauri/src/lib.rs:5531` (`mint_channel_create_event` signature + body)
- Modify: `src-tauri/src/lib.rs:5742` (`mint_channel_modify_event` signature + body)
- Modify: `src-tauri/src/lib.rs:5780` (`mint_channel_delete_event` signature + body)
- Modify: `src-tauri/src/lib.rs:6434` (`mint_community_creation` signature + body)
- Modify: `src-tauri/src/lib.rs:7187` (`mint_redemption` signature + body)
- Modify: `src-tauri/src/lib.rs:8235` (`mint_leave_event` signature + body)
- Modify: `src-tauri/src/lib.rs:8514` (`mint_kick_event` signature + body)
- Modify: `src-tauri/src/lib.rs:8676` (`mint_set_power_event` signature + body)
- Modify: `src-tauri/src/lib.rs:7035` (`mint_creation_produces_consistent_id_join_event_and_space` test)
- Modify: `src-tauri/src/lib.rs:8156` (`mint_redemption_produces_self_join_and_matching_space` test)
- Modify: `src-tauri/src/lib.rs:8458` (`mint_leave_produces_self_leave_event` test)
- Read-only / consume during port: any other `mint_*` unit tests in the same `cfg(test)` modules

- [ ] **Step 1: Re-read each `mint_*` helper to confirm its current signature**

Run: `grep -n "pub fn mint_" src-tauri/src/lib.rs`
Expected output (from current `main`):
```
5531:pub fn mint_channel_create_event(
5742:pub fn mint_channel_modify_event(
5780:pub fn mint_channel_delete_event(
6434:pub fn mint_community_creation(
7187:pub fn mint_redemption(
8235:pub fn mint_leave_event(
8514:pub fn mint_kick_event(
8676:pub fn mint_set_power_event(
```

If any line number drifts (e.g., due to other changes), use the function name to locate the helper, not the line number — names are stable.

- [ ] **Step 2: Refactor `mint_channel_create_event`**

Replace the entire function body at `src-tauri/src/lib.rs:5530-5562` (from `#[allow(clippy::too_many_arguments)]` through the closing `}`) with:

```rust
/// Pure function: mint a self-signed ChannelCreate event for a
/// community we belong to and have permission to moderate. Mirrors
/// `mint_kick_event` / `mint_set_power_event`. The fresh `channel_id`
/// (16 random bytes) and event id are sourced from the supplied RNG
/// (via `rand::thread_rng` in production).
///
/// ZEB-267: Caller pre-reserves `hlc` via
/// `dm_outbox::reserve_next_hlc_for_device`. This helper is now pure
/// on the HLC — it does not call `next_hlc` internally.
pub fn mint_channel_create_event(
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    channel_id: crate::community_membership::ChannelId,
    name: String,
    write_power: u8,
    signing_key: &ed25519_dalek::SigningKey,
    hlc: crate::owner_state_types::Hlc,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::ChannelCreate {
            channel_id,
            name,
            write_power,
        },
        actor: self_owner,
        at: hlc,
    };
    sign_event(&payload, signing_key).map_err(|e| format!("sign channel_create: {e}"))
}
```

The `#[allow(clippy::too_many_arguments)]` attribute is dropped because the new signature has 7 params (under clippy's default 7 threshold) — drop it cleanly so future drift doesn't reintroduce it silently.

- [ ] **Step 3: Refactor `mint_channel_modify_event`**

Replace the entire function body at `src-tauri/src/lib.rs:5741-5773` with:

```rust
/// Pure function: mint a self-signed ChannelModify event for a community
/// we moderate. Mirrors `mint_channel_create_event`. Caller is responsible
/// for ensuring at least one of `name`/`write_power` is `Some` (the IPC
/// boundary rejects all-None before this is reached).
///
/// ZEB-267: Caller pre-reserves `hlc` via
/// `dm_outbox::reserve_next_hlc_for_device`.
pub fn mint_channel_modify_event(
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    channel_id: crate::community_membership::ChannelId,
    name: Option<String>,
    write_power: Option<u8>,
    signing_key: &ed25519_dalek::SigningKey,
    hlc: crate::owner_state_types::Hlc,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::ChannelModify {
            channel_id,
            name,
            write_power,
        },
        actor: self_owner,
        at: hlc,
    };
    sign_event(&payload, signing_key).map_err(|e| format!("sign channel_modify: {e}"))
}
```

Same `#[allow(clippy::too_many_arguments)]` removal applies (this helper now has 7 params).

- [ ] **Step 4: Refactor `mint_channel_delete_event`**

Replace the entire function body at `src-tauri/src/lib.rs:5779-5805` with:

```rust
/// Pure function: mint a self-signed ChannelDelete event for a community
/// we moderate. Mirrors `mint_channel_create_event`. Caller is responsible
/// for the metadata-before-write check (channel exists + not already
/// tombstoned) — this helper does NOT validate; it only mints.
///
/// ZEB-267: Caller pre-reserves `hlc` via
/// `dm_outbox::reserve_next_hlc_for_device`.
pub fn mint_channel_delete_event(
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    channel_id: crate::community_membership::ChannelId,
    signing_key: &ed25519_dalek::SigningKey,
    hlc: crate::owner_state_types::Hlc,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::ChannelDelete { channel_id },
        actor: self_owner,
        at: hlc,
    };
    sign_event(&payload, signing_key).map_err(|e| format!("sign channel_delete: {e}"))
}
```

This helper now has 5 params, well under clippy's threshold. The `#[allow(clippy::too_many_arguments)]` attribute (originally on the old signature) gets dropped.

- [ ] **Step 5: Refactor `mint_community_creation`**

Replace the entire function body at `src-tauri/src/lib.rs:6434-6496` with:

```rust
/// Pure function: mint a fresh community + signed bootstrap Join.
///
/// Generates random `community_id` (16 bytes) and `MembershipKey`
/// (32 bytes), builds the Community Space row, signs a self-Join
/// `SignedMembershipEvent` with the caller's ed25519 key. Returns
/// all four artefacts so the IPC layer can apply the Space, send
/// the Join through the engine, and return the hex id to the frontend.
///
/// Pure / sync / no I/O — every random byte is sourced from the args.
/// This lets the test (`create_community_inner_tests`) cover the full
/// mint without spawning channels, mutexes, or a Tauri runtime.
///
/// ZEB-267: Caller pre-reserves `creation_hlc` via
/// `dm_outbox::reserve_next_hlc_for_device`.
pub fn mint_community_creation(
    name: &str,
    is_invite_only: bool,
    self_owner: crate::owner_state_types::OwnerAddr,
    signing_key: &ed25519_dalek::SigningKey,
    creation_hlc: crate::owner_state_types::Hlc,
) -> Result<MintedCommunity, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use crate::owner_state_types::{MembershipKey, Space, SpaceId, SpaceKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut id_bytes = [0u8; 16];
    rng.fill_bytes(&mut id_bytes);
    let community_id = SpaceId(id_bytes);

    let mut mk_bytes = [0u8; 32];
    rng.fill_bytes(&mut mk_bytes);
    let membership_key = MembershipKey::new(mk_bytes);

    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);
    let join_payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::Join,
        actor: self_owner,
        at: creation_hlc.clone(),
    };
    let bootstrap_join =
        sign_event(&join_payload, signing_key).map_err(|e| format!("sign bootstrap join: {e}"))?;

    let space = Space {
        id: community_id,
        kind: SpaceKind::Community,
        parent: None,
        community_id: None,
        name: name.to_string(),
        transport: None,
        members: Vec::new(),
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: creation_hlc.clone(),
        updated_at: creation_hlc,
        content_key: None,
        prior_content_keys: Vec::new(),
        membership_key: Some(membership_key.clone()),
        admin_addr: Some(self_owner),
        is_invite_only: Some(is_invite_only),
    };

    Ok(MintedCommunity {
        community_id,
        membership_key,
        space,
        bootstrap_join,
    })
}
```

Note: this helper's signature now has 5 params (down from 7). Drop the `#[allow(clippy::too_many_arguments)]` attribute that was on the old signature.

- [ ] **Step 6: Refactor `mint_redemption`**

Replace the entire function body at `src-tauri/src/lib.rs:7187-7250` with:

```rust
/// Pure function: builds the joiner-side `MintedCommunity` from an
/// invite payload — derives a Community Space row from `payload.name`
/// / `is_invite_only`, and signs a self-Join `SignedMembershipEvent`
/// (actor = `self_owner`, community_id = `payload.community_id`).
///
/// Pure / sync / no I/O — the caller supplies `join_hlc`. This lets
/// the test (`redeem_invite_inner_tests`) cover the full mint without
/// spawning channels, mutexes, or a Tauri runtime.
///
/// ZEB-267: Caller pre-reserves `join_hlc` via
/// `dm_outbox::reserve_next_hlc_for_device`.
pub fn mint_redemption(
    payload: &crate::community_invite::CommunityInvitePayload,
    self_owner: crate::owner_state_types::OwnerAddr,
    signing_key: &ed25519_dalek::SigningKey,
    join_hlc: crate::owner_state_types::Hlc,
) -> Result<MintedCommunity, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use crate::owner_state_types::{Space, SpaceKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let join_payload = EventPayload {
        id: event_id_bytes,
        community_id: payload.community_id,
        kind: MembershipEventKind::Join,
        actor: self_owner,
        at: join_hlc.clone(),
    };
    let bootstrap_join =
        sign_event(&join_payload, signing_key).map_err(|e| format!("sign self-join: {e}"))?;

    let space = Space {
        id: payload.community_id,
        kind: SpaceKind::Community,
        parent: None,
        community_id: None,
        name: payload.community_name.clone(),
        transport: None,
        members: Vec::new(),
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: join_hlc.clone(),
        updated_at: join_hlc,
        content_key: None,
        prior_content_keys: Vec::new(),
        membership_key: Some(payload.membership_key.clone()),
        admin_addr: Some(payload.admin_addr),
        // Use the invite's declared is_invite_only so the redeemer's
        // Space row matches the creator's row (Phase 1's CRDT same-
        // SpaceId rejection of community-creation field changes would
        // silently reject the redemption Space if these disagreed).
        is_invite_only: Some(payload.is_invite_only),
    };

    Ok(MintedCommunity {
        community_id: payload.community_id,
        membership_key: payload.membership_key.clone(),
        space,
        bootstrap_join,
    })
}
```

This helper had no `#[allow(clippy::too_many_arguments)]` attribute and now has 4 params — confirm that no `#[allow(...)]` attribute is added back.

- [ ] **Step 7: Refactor `mint_leave_event`**

Replace the entire function body at `src-tauri/src/lib.rs:8235-8259` with:

```rust
/// Pure function: mint a self-Leave `SignedMembershipEvent` for a
/// community we currently belong to. Mirrors the
/// `mint_redemption` / `mint_community_creation` shape — pure / sync /
/// no I/O so the canonical-CBOR / signing path is unit-testable
/// without standing up a Tauri test harness.
///
/// ZEB-267: Caller pre-reserves `hlc` via
/// `dm_outbox::reserve_next_hlc_for_device`.
pub fn mint_leave_event(
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    signing_key: &ed25519_dalek::SigningKey,
    hlc: crate::owner_state_types::Hlc,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::Leave,
        actor: self_owner,
        at: hlc,
    };
    sign_event(&payload, signing_key).map_err(|e| format!("sign leave: {e}"))
}
```

4 params, no `#[allow(...)]`.

- [ ] **Step 8: Refactor `mint_kick_event`**

Replace the entire function body at `src-tauri/src/lib.rs:8513-8540` with:

```rust
/// Pure function: mint a self-signed Kick event for a community we
/// belong to and have permission to moderate. Mirrors `mint_leave_event`.
///
/// ZEB-267: Caller pre-reserves `hlc` via
/// `dm_outbox::reserve_next_hlc_for_device`.
pub fn mint_kick_event(
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    target: crate::owner_state_types::OwnerAddr,
    reason: Option<String>,
    signing_key: &ed25519_dalek::SigningKey,
    hlc: crate::owner_state_types::Hlc,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::Kick { target, reason },
        actor: self_owner,
        at: hlc,
    };
    sign_event(&payload, signing_key).map_err(|e| format!("sign kick: {e}"))
}
```

6 params — drop the `#[allow(clippy::too_many_arguments)]` attribute that was on the old 8-param signature.

- [ ] **Step 9: Refactor `mint_set_power_event`**

Replace the entire function body at `src-tauri/src/lib.rs:8675-8702` with:

```rust
/// Pure function: mint a self-signed SetPower event for a community we
/// moderate (verify_event power-gates at level ≥ 100).
///
/// ZEB-267: Caller pre-reserves `hlc` via
/// `dm_outbox::reserve_next_hlc_for_device`.
pub fn mint_set_power_event(
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    target: crate::owner_state_types::OwnerAddr,
    level: u8,
    signing_key: &ed25519_dalek::SigningKey,
    hlc: crate::owner_state_types::Hlc,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::SetPower { target, level },
        actor: self_owner,
        at: hlc,
    };
    sign_event(&payload, signing_key).map_err(|e| format!("sign set_power: {e}"))
}
```

6 params — drop the `#[allow(clippy::too_many_arguments)]` attribute.

- [ ] **Step 10: Update `mint_creation_produces_consistent_id_join_event_and_space` test (line 7035)**

Find the test body at `src-tauri/src/lib.rs:7034-7118` (inside `mod create_community_inner_tests`). Replace the body. Two `mint_community_creation` call sites need updating:

```rust
    #[test]
    fn mint_creation_produces_consistent_id_join_event_and_space() {
        let identity = PrivateIdentity::from_seed(&[0xc1; 32]);
        let self_owner = OwnerAddr(identity.identity.address_hash);
        // Reach into the PrivateIdentity's signing path the same way
        // production does: the canonical 32-byte seed lives in bytes
        // 32..64 of `to_private_bytes()` (X25519_secret(32) ||
        // Ed25519_secret(32)). dm_outbox stores the SigningKey
        // constructed from those bytes; mirror that here so the test
        // signs with the same key the IPC will use in production.
        let sk_bytes_full = identity.to_private_bytes();
        let ed_seed: [u8; 32] = sk_bytes_full[32..64]
            .try_into()
            .expect("ed25519 seed slice 32..64");
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed_seed);

        let device_id = "creator-dev";
        let wall_now_ms = 1_700_000_000_000u64;
        // ZEB-267: caller pre-reserves the HLC; in production this
        // comes from `reserve_next_hlc_for_device`. The test constructs
        // it inline to keep the mint helper purely synchronous.
        let creation_hlc = Hlc {
            wall_ms: wall_now_ms,
            logical: 0,
            device_id: device_id.to_string(),
        };

        let minted = mint_community_creation(
            "Hackers United",
            false,
            self_owner,
            &signing_key,
            creation_hlc.clone(),
        )
        .expect("mint");

        assert_eq!(
            minted.space.kind,
            crate::owner_state_types::SpaceKind::Community
        );
        assert_eq!(minted.space.id, minted.community_id);
        assert_eq!(minted.space.admin_addr, Some(self_owner));
        assert_eq!(minted.space.is_invite_only, Some(false));
        assert!(minted.space.membership_key.is_some());
        assert_eq!(minted.space.name, "Hackers United");
        assert_eq!(minted.space.created_at.wall_ms, wall_now_ms);
        assert_eq!(&minted.space.created_at.device_id, device_id);

        assert_eq!(minted.bootstrap_join.actor, self_owner);
        assert_eq!(minted.bootstrap_join.community_id, minted.community_id);
        assert!(matches!(
            minted.bootstrap_join.kind,
            crate::community_membership::MembershipEventKind::Join
        ));
        assert_eq!(minted.bootstrap_join.at.wall_ms, wall_now_ms);
        assert!(
            minted.bootstrap_join.countersig.is_none(),
            "open / bootstrap Join carries no countersig"
        );

        // Two consecutive mints must produce DISTINCT community ids /
        // event ids / membership keys — the random source has to fire
        // per call, otherwise two communities created in a row would
        // collide. (16-byte / 32-byte randomness collision is
        // astronomically unlikely; this just guards against a
        // rng-reuse / fixed-buffer bug.)
        let minted2 = mint_community_creation(
            "Other Community",
            false,
            self_owner,
            &signing_key,
            creation_hlc.clone(),
        )
        .expect("mint2");
        assert_ne!(minted.community_id, minted2.community_id);
        assert_ne!(minted.bootstrap_join.id, minted2.bootstrap_join.id);
        assert_ne!(
            minted.space.membership_key.as_ref().unwrap().as_bytes(),
            minted2.space.membership_key.as_ref().unwrap().as_bytes(),
        );

        // Bootstrap signature MUST verify against self_owner's
        // identity_pub — the engine's verify_event will run the same
        // check on insert_local_event.
        let identity_pub = identity.identity.to_public_bytes();
        crate::community_membership::verify_signature(&minted.bootstrap_join, &identity_pub)
            .expect("bootstrap join signature must verify against self identity_pub");
    }
```

Note: The `prev_hlc: Option<Hlc> = None` binding is removed (no longer used). The `Hlc` import is already in scope via `use crate::owner_state_types::{Hlc, OwnerAddr};` at the top of the test mod.

- [ ] **Step 11: Update `mint_redemption_produces_self_join_and_matching_space` test (line 8156)**

Find the test at `src-tauri/src/lib.rs:8155-8210` (inside `mod redeem_invite_inner_tests`). Replace the body:

```rust
    #[test]
    fn mint_redemption_produces_self_join_and_matching_space() {
        let identity = PrivateIdentity::from_seed(&[0xab; 32]);
        let identity_pub = identity.identity.to_public_bytes();
        let self_owner = OwnerAddr(identity.identity.address_hash);
        // Mirror Task 9's test pattern: pull the canonical 32-byte
        // Ed25519 seed from bytes 32..64 of `to_private_bytes()`. The
        // production IPC borrows this same SigningKey from `dm_outbox`.
        let sk_bytes_full = identity.to_private_bytes();
        let ed_seed: [u8; 32] = sk_bytes_full[32..64]
            .try_into()
            .expect("ed25519 seed slice 32..64");
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed_seed);

        let payload = CommunityInvitePayload {
            community_id: SpaceId([0xee; 16]),
            membership_key: MembershipKey::new([0x77; 32]),
            admin_addr: OwnerAddr([0x33; 16]),
            community_name: "TestCom".into(),
            is_invite_only: false,
            expires_at: None,
            invite_token: None,
            admin_bootstrap: None,
            admin_identity_pub: None,
        };

        let device_id = "joiner-dev";
        // ZEB-267: caller pre-reserves the HLC; constructed inline here
        // since the test isn't driving an actual tracker.
        let join_hlc = Hlc {
            wall_ms: 1_700_000_999_000u64,
            logical: 0,
            device_id: device_id.to_string(),
        };

        let minted = mint_redemption(&payload, self_owner, &signing_key, join_hlc).expect("mint");

        assert_eq!(minted.community_id, payload.community_id);
        assert_eq!(minted.space.id, payload.community_id);
        assert_eq!(minted.space.admin_addr, Some(payload.admin_addr));
        assert_eq!(minted.space.is_invite_only, Some(false));
        assert_eq!(minted.bootstrap_join.actor, self_owner);
        assert_eq!(minted.bootstrap_join.community_id, payload.community_id);
        assert!(matches!(
            minted.bootstrap_join.kind,
            crate::community_membership::MembershipEventKind::Join
        ));

        // Self-join sig must verify against the joiner's identity_pub —
        // the engine's verify_event runs the same check on insert.
        crate::community_membership::verify_signature(&minted.bootstrap_join, &identity_pub)
            .expect("self-join signature must verify against joiner identity_pub");
    }
```

The `wall_now_ms` and `prev_hlc` bindings are removed. The `Hlc` import is already in scope.

- [ ] **Step 12: Update `mint_leave_produces_self_leave_event` test (line 8458)**

Find the test at `src-tauri/src/lib.rs:8457-8498` (inside `mod leave_community_inner_tests`). Replace the body:

```rust
    #[test]
    fn mint_leave_produces_self_leave_event() {
        let identity = PrivateIdentity::from_seed(&[0xab; 32]);
        let identity_pub = identity.identity.to_public_bytes();
        let self_owner = OwnerAddr(identity.identity.address_hash);
        // Mirror Task 9/10's test pattern: pull the canonical 32-byte
        // Ed25519 seed from bytes 32..64 of `to_private_bytes()`. The
        // production IPC borrows this same SigningKey from `dm_outbox`.
        let sk_bytes_full = identity.to_private_bytes();
        let ed_seed: [u8; 32] = sk_bytes_full[32..64]
            .try_into()
            .expect("ed25519 seed slice 32..64");
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed_seed);

        let community_id = SpaceId([0x77; 16]);
        let device_id = "leaver-dev";
        let wall_now_ms = 1_700_000_500_000u64;
        // ZEB-267: caller pre-reserves the HLC.
        let leave_hlc = Hlc {
            wall_ms: wall_now_ms,
            logical: 0,
            device_id: device_id.to_string(),
        };

        let event = mint_leave_event(community_id, self_owner, &signing_key, leave_hlc).expect("mint");

        assert_eq!(event.actor, self_owner);
        assert_eq!(event.community_id, community_id);
        assert!(matches!(
            event.kind,
            crate::community_membership::MembershipEventKind::Leave
        ));
        assert_eq!(event.at.wall_ms, wall_now_ms);

        // Self-Leave sig must verify against the leaver's identity_pub —
        // the engine's verify_event runs the same check on insert.
        crate::community_membership::verify_signature(&event, &identity_pub)
            .expect("self-leave signature must verify against leaver identity_pub");
    }
```

- [ ] **Step 13: Search for any other in-tree mint_*_event test callers and update**

Run: `grep -n "mint_kick_event\|mint_set_power_event\|mint_channel_create_event\|mint_channel_modify_event\|mint_channel_delete_event\|mint_community_creation\|mint_redemption\|mint_leave_event" src-tauri/src/lib.rs src-tauri/tests/*.rs`

Expected: a list of every callsite. The IPC sites in `lib.rs` are out of scope for this task (Task 3 fixes them). For each callsite NOT inside an IPC body, port it to the new signature.

Known callsites in `src-tauri/tests/*.rs`:
- `src-tauri/tests/community_sync_integration.rs:2023` — imports `mint_community_creation, mint_kick_event, mint_redemption, mint_set_power_event`. The body uses each at lines ~2146, ~2263, ~2310, ~2362. Each call passes `(..., wall_now_ms, prev_hlc)` today; port to `(..., hlc)` constructed inline.

Open `src-tauri/tests/community_sync_integration.rs` and update the four call sites:

At line ~2146 (inside `build_fixture`), replace:
```rust
        let minted_a = mint_community_creation(
            "TestCommunity",
            false,
            owner_a,
            &signing_a,
            "a-dev",
            100_000,
            None,
        )
        .expect("mint create");
```
with:
```rust
        let minted_a = mint_community_creation(
            "TestCommunity",
            false,
            owner_a,
            &signing_a,
            Hlc {
                wall_ms: 100_000,
                logical: 0,
                device_id: "a-dev".to_string(),
            },
        )
        .expect("mint create");
```

At line ~2263, replace:
```rust
        let minted_b =
            mint_redemption(&invite_payload, owner_b, &signing_b, "b-dev", 200_000, None)
                .expect("mint redeem");
```
with:
```rust
        let minted_b = mint_redemption(
            &invite_payload,
            owner_b,
            &signing_b,
            Hlc {
                wall_ms: 200_000,
                logical: 0,
                device_id: "b-dev".to_string(),
            },
        )
        .expect("mint redeem");
```

At line ~2310 (inside `admin_kicks_member_round_trip`), replace:
```rust
        let kick = mint_kick_event(
            f.community_id,
            f.owner_a,
            f.owner_b,
            Some("test-kick".into()),
            &f.signing_a,
            "a-dev",
            300_000,
            Some(&f.minted_b_join_hlc),
        )
        .expect("mint kick");
```
with:
```rust
        // ZEB-267: derive the kick's HLC from the most-recent observed
        // event (B's redemption Join), bumping logical to preserve
        // strict ordering. Production goes through
        // reserve_next_hlc_for_device against a tracker; this test mints
        // directly at engine level.
        let kick_hlc = Hlc {
            wall_ms: f.minted_b_join_hlc.wall_ms.max(300_000),
            logical: if f.minted_b_join_hlc.wall_ms >= 300_000 {
                f.minted_b_join_hlc.logical + 1
            } else {
                0
            },
            device_id: "a-dev".to_string(),
        };
        let kick = mint_kick_event(
            f.community_id,
            f.owner_a,
            f.owner_b,
            Some("test-kick".into()),
            &f.signing_a,
            kick_hlc,
        )
        .expect("mint kick");
```

At line ~2362 (inside `admin_sets_power_round_trip`), replace:
```rust
        let promo = mint_set_power_event(
            f.community_id,
            f.owner_a,
            f.owner_b,
            50,
            &f.signing_a,
            "a-dev",
            300_000,
            Some(&f.minted_b_join_hlc),
        )
        .expect("mint set_power");
```
with:
```rust
        // ZEB-267: same HLC derivation as the kick test above.
        let promo_hlc = Hlc {
            wall_ms: f.minted_b_join_hlc.wall_ms.max(300_000),
            logical: if f.minted_b_join_hlc.wall_ms >= 300_000 {
                f.minted_b_join_hlc.logical + 1
            } else {
                0
            },
            device_id: "a-dev".to_string(),
        };
        let promo = mint_set_power_event(
            f.community_id,
            f.owner_a,
            f.owner_b,
            50,
            &f.signing_a,
            promo_hlc,
        )
        .expect("mint set_power");
```

Confirm `Hlc` is imported in `community_sync_integration.rs` — search `grep -n "use.*Hlc" src-tauri/tests/community_sync_integration.rs`. If not imported, add `Hlc` to the existing `harmony_app::owner_state_types::*` use group near the top of the file.

- [ ] **Step 14: Search for any OTHER mint_* callsites in tests (defensive sweep)**

Run: `grep -rn "mint_redemption\|mint_community_creation\|mint_leave_event\|mint_kick_event\|mint_set_power_event\|mint_channel_create_event\|mint_channel_modify_event\|mint_channel_delete_event" src-tauri/tests/ src-tauri/src/`

Verify every callsite NOT inside an IPC function body in `lib.rs` (i.e., every test) has been updated. If you find any remaining callsites with the old signature, port them using the same `Hlc { wall_ms: ..., logical: 0, device_id: ... }` pattern.

- [ ] **Step 15: Run cargo check to confirm test code compiles (IPC sites still won't)**

Run: `cd src-tauri && cargo check --lib --tests 2>&1 | tee /tmp/zeb267-task2-check.log; echo "check exit: ${PIPESTATUS[0]}"`
Expected: NON-ZERO exit. Build errors should ALL be in `lib.rs` IPC bodies (the 9 sites Task 3 fixes), pointing at the now-removed `wall_now_ms` / `device_id` / `prev_hlc` parameters. NO error should reference `src-tauri/tests/*.rs` — if it does, you missed a callsite in Step 13/14, fix it before proceeding.

This is the "intentionally broken IPC sites, all tests compile" expected state.

- [ ] **Step 16: Run unit tests for the mint helpers in isolation**

Run: `cd src-tauri && cargo test --lib mint_creation_produces_consistent_id_join_event_and_space mint_redemption_produces_self_join_and_matching_space mint_leave_produces_self_leave_event 2>&1 | tee /tmp/zeb267-task2-mint-tests.log; echo "test exit: ${PIPESTATUS[0]}"`
Expected: NON-ZERO exit because the IPC bodies in `lib.rs` don't compile. The lib build itself fails before any test runs. This is expected for Task 2 — Task 3 makes the workspace fully compile.

- [ ] **Step 17: Run cargo fmt to keep formatting clean for the task commit**

Run: `cd src-tauri && cargo fmt --all`
Expected: exit 0; this normalizes any whitespace drift introduced during the signature edits.

- [ ] **Step 18: Verify no clippy regression on the helpers themselves**

Skip clippy at this checkpoint — clippy runs the full workspace lint pass and will fail on the broken IPC sites in `lib.rs`. We'll re-run clippy after Task 3 makes the workspace compile again. Note this in the commit message.

- [ ] **Step 19: Commit**

Run:
```bash
git add src-tauri/src/lib.rs src-tauri/tests/community_sync_integration.rs
git commit -m "$(cat <<'EOF'
refactor(zeb-267): mint_* helpers take pre-reserved Hlc directly

Replaces the (wall_now_ms, device_id, prev_hlc) parameter trio with
a single hlc: Hlc on each of the eight mint_* membership-event
helpers in lib.rs:

  - mint_channel_create_event
  - mint_channel_modify_event
  - mint_channel_delete_event
  - mint_community_creation
  - mint_redemption
  - mint_leave_event
  - mint_kick_event
  - mint_set_power_event

Helpers no longer call next_hlc internally; they're now pure on the
HLC. Caller is responsible for reserving via
dm_outbox::reserve_next_hlc_for_device (Task 1) before calling.
Drops several #[allow(clippy::too_many_arguments)] attributes that
were on the old signatures.

Three local unit tests in lib.rs::tests updated to construct an Hlc
inline. Four mint_* callsites in tests/community_sync_integration.rs
also ported to the new signature. Helpers' callsites in lib.rs IPC
bodies (kick_from_community, leave_community, set_power_level,
create_channel, modify_channel, delete_channel, redeem_invite_inner,
create_community_inner) are intentionally broken at this commit —
Task 3 wires them through reserve_next_hlc_for_device and fixes the
signature mismatch. cargo check fails on lib.rs IPC bodies; cargo
clippy is deferred until Task 3 (clippy needs a compiling lib).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 20: Verify the commit landed**

Run: `git log --oneline -3`
Expected: HEAD is the just-created Task 2 commit; Task 1 commit is second.

---

## Task 3: IPC call-site refactor (9 reservation sites)

**Goal:** At each of the nine reservation sites across eight membership-event IPCs, replace the snapshot-then-release block with a single `reserve_next_hlc_for_device` call, pass the reserved `Hlc` to the (now-simplified) mint helper, and delete the `if matches!(outcome, InsertOutcome::Inserted) { tracker.insert(...) }` post-block. After this task the workspace compiles green.

**Files:**
- Modify: `src-tauri/src/lib.rs:5645-5664` (`create_channel` mint+sign block)
- Modify: `src-tauri/src/lib.rs:5710-5725` (`create_channel` post-Inserted advance)
- Modify: `src-tauri/src/lib.rs:5894-5913` (`modify_channel` mint+sign block)
- Modify: `src-tauri/src/lib.rs:5957-5969` (`modify_channel` post-Inserted advance)
- Modify: `src-tauri/src/lib.rs:6125-6142` (`delete_channel` mint+sign block)
- Modify: `src-tauri/src/lib.rs:6182-6194` (`delete_channel` post-Inserted advance)
- Modify: `src-tauri/src/lib.rs:6554-6573` (`create_community_inner` bootstrap mint)
- Modify: `src-tauri/src/lib.rs:6709-6717` (`create_community_inner` chained default-channel mint)
- Modify: `src-tauri/src/lib.rs:6914-6915` (`create_community_inner` post-tracker advance — replaced by atomic reservation)
- Modify: `src-tauri/src/lib.rs:7313-7327` (`redeem_invite_inner` reservation + mint)
- Modify: `src-tauri/src/lib.rs:7948-7949` (`redeem_invite_inner` post-tracker advance — replaced)
- Modify: `src-tauri/src/lib.rs:8359-8374` (`leave_community` mint+sign block)
- Modify: `src-tauri/src/lib.rs:8419-8425` (`leave_community` post-Inserted advance)
- Modify: `src-tauri/src/lib.rs:8592-8609` (`kick_from_community` mint+sign block)
- Modify: `src-tauri/src/lib.rs:8655-8662` (`kick_from_community` post-Inserted advance)
- Modify: `src-tauri/src/lib.rs:8754-8771` (`set_power_level` mint+sign block)
- Modify: `src-tauri/src/lib.rs:8813-8819` (`set_power_level` post-Inserted advance)

- [ ] **Step 1: `kick_from_community` — replace mint block**

Find the block at `src-tauri/src/lib.rs:8592-8609`. Replace:

```rust
    // Mint under HLC tracker lock then drop the guard.
    let kick = {
        let prev_hlc = {
            let t = hlc_tracker.lock().await;
            t.get(&device_id).cloned()
        };
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        mint_kick_event(
            space_id,
            self_owner,
            target,
            reason,
            signing_key,
            &device_id,
            wall_now_ms,
            prev_hlc.as_ref(),
        )?
    };
```

with:

```rust
    // ZEB-267: reserve the HLC atomically (read-bump-write under
    // tracker lock) BEFORE minting. Replaces the prior
    // snapshot-then-release pattern that had a race window between
    // the prev_hlc read and the post-Inserted advance.
    let kick_hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;
    let kick = {
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        mint_kick_event(space_id, self_owner, target, reason, signing_key, kick_hlc)?
    };
```

- [ ] **Step 2: `kick_from_community` — delete the post-Inserted tracker advance**

Find the block at `src-tauri/src/lib.rs:8655-8662`. Delete:

```rust
    // Advance HLC tracker only on `Inserted` (mirrors leave_community).
    if matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Inserted
    ) {
        let mut t = hlc_tracker.lock().await;
        t.insert(device_id.clone(), kick.at.clone());
    }
```

The block sits between the rejection-handling `if matches!(outcome, ...::Rejected(_))` block and the function's closing `Ok(())` return. After deletion, the function flows from the rejection check straight to `Ok(())`.

- [ ] **Step 3: `set_power_level` — replace mint block**

Find the block at `src-tauri/src/lib.rs:8754-8771`. Replace:

```rust
    let event = {
        let prev_hlc = {
            let t = hlc_tracker.lock().await;
            t.get(&device_id).cloned()
        };
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        mint_set_power_event(
            space_id,
            self_owner,
            target,
            level,
            signing_key,
            &device_id,
            wall_now_ms,
            prev_hlc.as_ref(),
        )?
    };
```

with:

```rust
    // ZEB-267: atomic HLC reservation.
    let hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;
    let event = {
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        mint_set_power_event(space_id, self_owner, target, level, signing_key, hlc)?
    };
```

- [ ] **Step 4: `set_power_level` — delete the post-Inserted tracker advance**

Find the block at `src-tauri/src/lib.rs:8813-8819`. Delete:

```rust
    if matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Inserted
    ) {
        let mut t = hlc_tracker.lock().await;
        t.insert(device_id.clone(), event.at.clone());
    }

```

After deletion, the function's rejection-check is followed directly by `Ok(())`.

- [ ] **Step 5: `leave_community` — replace mint block**

Find the block at `src-tauri/src/lib.rs:8359-8374`. Replace:

```rust
    let leave = {
        let prev_hlc = {
            let t = hlc_tracker.lock().await;
            t.get(&device_id).cloned()
        };
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        mint_leave_event(
            space_id,
            self_owner,
            signing_key,
            &device_id,
            wall_now_ms,
            prev_hlc.as_ref(),
        )?
    };
```

with:

```rust
    // ZEB-267: atomic HLC reservation.
    let leave_hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;
    let leave = {
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        mint_leave_event(space_id, self_owner, signing_key, leave_hlc)?
    };
```

- [ ] **Step 6: `leave_community` — delete the post-Inserted tracker advance**

Find the block at `src-tauri/src/lib.rs:8419-8425`. Delete:

```rust
    // Advance HLC tracker only on `Inserted`. `AlreadyKnown` is benign
    // (the event we minted matches one the engine already had, so the
    // tracker is at-or-past `leave.at`), but advancing on it would
    // diverge from the principle the rest of the IPCs follow:
    // "advance HLC AFTER successful insert so failures don't bump
    // tracker". Cursor Bugbot LOW finding on PR #87 round 2.
    if matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Inserted
    ) {
        let mut t = hlc_tracker.lock().await;
        t.insert(device_id.clone(), leave.at.clone());
    }

```

After deletion, the function's rejection-check flows directly to the `app.emit("nav-updated", ...)` block.

- [ ] **Step 7: `create_channel` — replace mint block**

Find the block at `src-tauri/src/lib.rs:5645-5664`. Replace:

```rust
    // Mint under HLC tracker + dm_outbox locks then drop the guards.
    let event = {
        let prev_hlc = {
            let t = hlc_tracker.lock().await;
            t.get(&device_id).cloned()
        };
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        mint_channel_create_event(
            space_id,
            self_owner,
            channel_id,
            name,
            write_power,
            signing_key,
            &device_id,
            wall_now_ms,
            prev_hlc.as_ref(),
        )?
    };
```

with:

```rust
    // ZEB-267: atomic HLC reservation.
    let hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;
    let event = {
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        mint_channel_create_event(
            space_id,
            self_owner,
            channel_id,
            name,
            write_power,
            signing_key,
            hlc,
        )?
    };
```

- [ ] **Step 8: `create_channel` — restructure post-insert block (was a tracker advance + Ok(hex)/Err)**

Find the block at `src-tauri/src/lib.rs:5706-5725`:

```rust
    // Advance HLC tracker only on `Inserted` (mirrors the other Phase 4
    // mod-tier IPCs). `AlreadyKnown` is a 16-byte-event-id collision —
    // vanishingly unlikely; surface as Err so the caller knows the
    // channel wasn't created (the new channel_id we generated is gone).
    if matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Inserted
    ) {
        let mut t = hlc_tracker.lock().await;
        t.insert(device_id, event.at);
        Ok(hex::encode(channel_id.0))
    } else {
        // Outcome is AlreadyKnown — the engine already knows this exact
        // event (event_id collision). Vanishingly unlikely, but surface
        // it so the caller doesn't think the channel was created.
        Err(format!(
            "create_channel unexpected outcome: AlreadyKnown (event_id collision: {})",
            hex::encode(event.id)
        ))
    }
```

Replace with (keeping the AlreadyKnown error path; just dropping the tracker advance from the Inserted arm):

```rust
    // ZEB-267: tracker is bumped at reservation time, so no post-Inserted
    // advance here. AlreadyKnown is a 16-byte-event-id collision —
    // vanishingly unlikely; surface as Err so the caller knows the
    // channel wasn't created (the new channel_id we generated is gone).
    if matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Inserted
    ) {
        Ok(hex::encode(channel_id.0))
    } else {
        Err(format!(
            "create_channel unexpected outcome: AlreadyKnown (event_id collision: {})",
            hex::encode(event.id)
        ))
    }
```

- [ ] **Step 9: `modify_channel` — replace mint block**

Find the block at `src-tauri/src/lib.rs:5894-5913`. Replace:

```rust
    // Mint under HLC tracker + dm_outbox locks then drop the guards.
    let event = {
        let prev_hlc = {
            let t = hlc_tracker.lock().await;
            t.get(&device_id).cloned()
        };
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        mint_channel_modify_event(
            space_id,
            self_owner,
            channel_id,
            name,
            write_power,
            signing_key,
            &device_id,
            wall_now_ms,
            prev_hlc.as_ref(),
        )?
    };
```

with:

```rust
    // ZEB-267: atomic HLC reservation.
    let hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;
    let event = {
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        mint_channel_modify_event(
            space_id,
            self_owner,
            channel_id,
            name,
            write_power,
            signing_key,
            hlc,
        )?
    };
```

- [ ] **Step 10: `modify_channel` — restructure post-insert block**

Find the block at `src-tauri/src/lib.rs:5954-5969`:

```rust
    // Advance HLC tracker only on `Inserted` (mirrors create_channel).
    // `AlreadyKnown` is a 16-byte-event-id collision — vanishingly
    // unlikely; surface as Err so the caller knows the modify didn't apply.
    if matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Inserted
    ) {
        let mut t = hlc_tracker.lock().await;
        t.insert(device_id, event.at);
        Ok(())
    } else {
        Err(format!(
            "modify_channel unexpected outcome: AlreadyKnown (event_id collision: {})",
            hex::encode(event.id)
        ))
    }
```

Replace with:

```rust
    // ZEB-267: tracker is bumped at reservation time. AlreadyKnown is
    // a 16-byte-event-id collision — vanishingly unlikely; surface as
    // Err so the caller knows the modify didn't apply.
    if matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Inserted
    ) {
        Ok(())
    } else {
        Err(format!(
            "modify_channel unexpected outcome: AlreadyKnown (event_id collision: {})",
            hex::encode(event.id)
        ))
    }
```

- [ ] **Step 11: `delete_channel` — replace mint block**

Find the block at `src-tauri/src/lib.rs:6125-6142`. Replace:

```rust
    // Mint under HLC tracker + dm_outbox locks then drop the guards.
    let event = {
        let prev_hlc = {
            let t = hlc_tracker.lock().await;
            t.get(&device_id).cloned()
        };
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        mint_channel_delete_event(
            space_id,
            self_owner,
            channel_id,
            signing_key,
            &device_id,
            wall_now_ms,
            prev_hlc.as_ref(),
        )?
    };
```

with:

```rust
    // ZEB-267: atomic HLC reservation. Note that the reservation
    // happens AFTER the metadata-before-irreversible-write read at
    // step 6 above (the channel-exists / not-tombstoned check) per
    // user memory rule — burning an HLC on a stale read-side rejection
    // is fine, but burning an HLC inside an actually-no-op event is
    // worse UX (caller pays for an HLC tick they can't see).
    let hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;
    let event = {
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        mint_channel_delete_event(space_id, self_owner, channel_id, signing_key, hlc)?
    };
```

- [ ] **Step 12: `delete_channel` — restructure post-insert block**

Find the block at `src-tauri/src/lib.rs:6182-6194`:

```rust
    if matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Inserted
    ) {
        let mut t = hlc_tracker.lock().await;
        t.insert(device_id, event.at);
        Ok(())
    } else {
        Err(format!(
            "delete_channel unexpected outcome: AlreadyKnown (event_id collision: {})",
            hex::encode(event.id)
        ))
    }
```

Replace with:

```rust
    // ZEB-267: tracker bumped at reservation time. AlreadyKnown is a
    // 16-byte-event-id collision — vanishingly unlikely; surface as
    // Err so the caller knows the delete didn't apply.
    if matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Inserted
    ) {
        Ok(())
    } else {
        Err(format!(
            "delete_channel unexpected outcome: AlreadyKnown (event_id collision: {})",
            hex::encode(event.id)
        ))
    }
```

- [ ] **Step 13: `create_community_inner` — replace bootstrap mint block**

Find the block at `src-tauri/src/lib.rs:6553-6573`. Replace:

```rust
    // Mint the Space + signed bootstrap Join. Read prev_hlc under the
    // tracker lock then drop the guard before signing (sign is sync;
    // releasing eagerly keeps the tracker available to other tasks).
    // ZEB-258: NO mutation of owner-state or hlc_tracker yet — the mint
    // is pure / sync and produces values, not side effects.
    let minted = {
        let prev_hlc = {
            let t = hlc_tracker.lock().await;
            t.get(&device_id).cloned()
        };
        mint_community_creation(
            &name,
            is_invite_only,
            self_owner,
            signing_key.as_ref(),
            &device_id,
            wall_now_ms,
            prev_hlc.as_ref(),
        )?
    };
```

with:

```rust
    // ZEB-267: atomic HLC reservation. The tracker is bumped here
    // (reservation time), not at the post-commit `tracker_g.insert`
    // line that ZEB-258 originally placed inside the apply_space
    // critical section. Burn semantics: if owner-state apply_space
    // rejects later, the reserved HLC is "burned" — fine, since
    // HLCs are 64-bit logical and the burn-on-rollback shape is
    // already implicit on the engine-spawn / adapter-dispatch
    // failure paths above. ZEB-258's atomicity property (Space row
    // commit is the LAST persistent step) is preserved — the
    // tracker advance is no longer co-located with the apply_space
    // call, but tracker advance was always orthogonal to the
    // owner-state Space-row write (they both persist via
    // persist_both, but the tracker is a per-device monotone
    // counter, not a Space-row field).
    //
    // BREAKING WITH ZEB-258 NOTE: the commented invariant at the
    // matching tracker_g.insert site below ("hold both guards
    // across the apply+insert pair") is no longer load-bearing for
    // tracker monotonicity since the reservation is atomic. We
    // still hold state_g across the apply for owner-state rollback
    // semantics, but tracker_g is gone from that block.
    let creation_hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;
    let minted = mint_community_creation(
        &name,
        is_invite_only,
        self_owner,
        signing_key.as_ref(),
        creation_hlc,
    )?;
```

- [ ] **Step 14: `create_community_inner` — replace chained default-channel mint block**

Find the block at `src-tauri/src/lib.rs:6709-6717`. Replace:

```rust
    // Use `bootstrap_join.at.wall_ms` as the wall input so the helper
    // returns `(bootstrap.wall_ms, bootstrap.logical+1, bootstrap.device_id)`
    // — the same deterministic ordering as before, but via the shared
    // `next_hlc` path (consistent with all other mint sites in lib.rs).
    let default_channel_at = crate::dm_outbox::next_hlc(
        Some(&minted.bootstrap_join.at),
        minted.bootstrap_join.at.wall_ms,
        &minted.bootstrap_join.at.device_id,
    );
```

with:

```rust
    // ZEB-267: reserve a SECOND HLC atomically. The tracker was just
    // bumped to `bootstrap_join.at` by the first reservation above,
    // so this reservation reads tracker == bootstrap_join.at and
    // returns a strictly-greater HLC. next_hlc's wall-clock
    // regression handling means the result is
    // `(bootstrap.wall_ms, bootstrap.logical+1, bootstrap.device_id)`
    // when wall_now_ms == bootstrap_join.at.wall_ms (the typical
    // case — same `wall_now_ms` value from the caller's snapshot),
    // bit-identical to the chained next_hlc call this replaces.
    // Burn semantics: same as the first reservation — if a downstream
    // step fails, the burned HLC is fine.
    let default_channel_at =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;
```

- [ ] **Step 15: `create_community_inner` — remove the post-commit tracker advance from the apply_space block**

Find the block at `src-tauri/src/lib.rs:6876-6919`. The current block holds `state_g` across the `tracker_g.insert(...)` call. With ZEB-267, the tracker is already bumped from the reservations at Steps 13-14, so the `tracker_g.insert(...)` line at 6915 is redundant.

Delete the section from `let mut tracker_g = hlc_tracker.lock().await;` through `tracker_g.insert(...)` (inclusive). The surrounding block stays — we still hold `state_g` across the `apply_space_with_canonicalization` call for owner-state-row commit semantics; we just remove the tracker advance.

Specifically, replace:

```rust
        // Success: acquire tracker WHILE still holding state_g, so
        // any concurrent persist_both blocks at `state.lock()` until
        // both writes are committed. Bootstrap creation: prev_hlc
        // was either None or strictly older than the latest event,
        // so the insert is monotonic.
        //
        // Persist the HIGHER of the two HLCs we minted in this
        // transaction (default-channel at `bootstrap.logical+1`),
        // not `minted.space.created_at` (= bootstrap_join.at, with
        // `logical=0`). Otherwise the tracker regresses below the
        // last-minted event and the next locally-minted event could
        // collide with the default-channel HLC at the same
        // (wall_ms, device_id).
        let mut tracker_g = hlc_tracker.lock().await;
        tracker_g.insert(device_id.clone(), default_channel_at.clone());
        // Both guards drop here at scope end, in reverse acquisition
        // order (tracker, then state) — neutral wrt other call sites
        // but consistent with Rust's drop semantics.
    }
```

with:

```rust
        // ZEB-267: tracker advance no longer needed here — the two
        // reservations above (bootstrap_join + default-channel)
        // already bumped the tracker atomically. state_g drops at
        // scope end. Burn semantics (if apply_space had rejected
        // earlier in this block): the reserved HLCs are fine to
        // "burn" — HLCs are 64-bit logical, not finite.
    }
```

The `default_channel_at` binding from Step 14 is still used a few lines up (in `default_channel_payload`), but `device_id` may now produce an unused-variable warning if no other line in this scope uses it. Verify by reading the surrounding lines — `device_id` is used many places in `create_community_inner`, so this is unlikely to be an issue. If `cargo build` produces an unused-variable warning, the fix is to inspect usage and either remove the unused var or prefix with `_`.

- [ ] **Step 16: `redeem_invite_inner` — replace reservation + mint block**

Find the block at `src-tauri/src/lib.rs:7313-7327`. Replace:

```rust
    // 4. Reserve HLC under tracker lock.
    let prev_hlc = {
        let t = hlc_tracker.lock().await;
        t.get(&device_id).cloned()
    };

    // 5. Mint (pure helper — no side effects on owner-state yet).
    let minted = mint_redemption(
        &payload,
        self_owner,
        signing_key.as_ref(),
        &device_id,
        wall_now_ms,
        prev_hlc.as_ref(),
    )?;
```

with:

```rust
    // 4. ZEB-267: atomic HLC reservation. Replaces the
    //    snapshot-then-release pattern + post-commit advance.
    let join_hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;

    // 5. Mint (pure helper — no side effects on owner-state yet).
    let minted = mint_redemption(&payload, self_owner, signing_key.as_ref(), join_hlc)?;
```

- [ ] **Step 17: `redeem_invite_inner` — remove the post-commit tracker advance**

Find the block at `src-tauri/src/lib.rs:7945-7950`. Replace:

```rust
        // Success: acquire tracker WHILE still holding state_g.
        // Bootstrap creation: prev_hlc was either None or strictly
        // older than `created_at`, so the insert is monotonic.
        let mut tracker_g = hlc_tracker.lock().await;
        tracker_g.insert(device_id.clone(), minted.space.created_at.clone());
        // Both guards drop here at scope end.
    }
```

with:

```rust
        // ZEB-267: tracker advance no longer needed here — the
        // reservation at step 4 already bumped the tracker atomically.
        // state_g drops at scope end.
    }
```

The doc comment block at lines 7330-7338 (about deferring tracker advance until after Space commit) is now stale. Update it:

Find:

```rust
    // ZEB-258 atomicity: the HLC tracker is persisted to
    // `state_root_replay.cbor` alongside the CRDT — advancing it here
    // (before engine spawn, adapter dispatch, the bootstrap-Join
    // insert, the invite-only oneshot dance, the fence check, AND the
    // final apply_space) would mean any rollback path leaves a
    // phantom tracker entry on disk with no matching Space row. The
    // advance is deferred until AFTER the Space commit succeeds at
    // step 9 — see the matching `tracker_g.insert` immediately after
    // the apply_space block.
```

Replace with:

```rust
    // ZEB-267 (replaces the prior ZEB-258 comment): the HLC tracker
    // is bumped at reservation time (step 4) regardless of whether
    // owner-state apply_space succeeds at step 9. Burn semantics:
    // a reserved HLC on a rollback path is "burned" — fine, since
    // HLCs are 64-bit logical and burn-on-rollback is already the
    // implicit behavior on the engine-spawn / adapter-dispatch
    // failure paths above. The original ZEB-258 concern (phantom
    // tracker entry without matching Space row) was about
    // persistence atomicity; advancing the tracker without persisting
    // the Space leaves a stale tracker entry, but that's a benign
    // consistency drift — the tracker is a per-device monotone
    // counter, not a constraint on the Space-row map.
```

- [ ] **Step 18: Verify the workspace compiles**

Run: `cd src-tauri && cargo check --lib --tests 2>&1 | tee /tmp/zeb267-task3-check.log; echo "check exit: ${PIPESTATUS[0]}"`
Expected: final line `check exit: 0`. If non-zero, scan the output for compile errors and fix them inline before running the next step. Common issues:
- Unused `device_id` binding in `create_community_inner` (prefix with `_` or leave — `device_id` is used elsewhere in that function so should be fine)
- Unused `wall_now_ms` binding in any IPC if the only use was the deleted snapshot block (highly unlikely — `wall_now_ms` flows into the reservation call)

- [ ] **Step 19: Run cargo fmt**

Run: `cd src-tauri && cargo fmt --all`
Expected: no output, exit 0.

- [ ] **Step 20: Run cargo clippy across the workspace**

Run: `cd src-tauri && cargo clippy --all-targets -- -D warnings 2>&1 | tee /tmp/zeb267-task3-clippy.log; echo "clippy exit: ${PIPESTATUS[0]}"`
Expected: final line `clippy exit: 0`. If clippy emits warnings on the new code, address them inline (e.g., `#[allow(clippy::unused_async)]` is NOT allowed — fix the underlying issue or use `.await` correctly; `clippy::needless_borrow` may flag `&hlc_tracker` if I'm passing it where it auto-derefs).

- [ ] **Step 21: Run the full workspace test suite**

Run: `cd src-tauri && cargo test --workspace --no-fail-fast 2>&1 | tee /tmp/zeb267-task3-tests.log; echo "test exit: ${PIPESTATUS[0]}"`
Expected: final line `test exit: 0`. The test count should be Task 0 baseline + 3 (the new helper tests from Task 1).

If any test fails, read its name and message:
- Tests in `community_open_flow_integration.rs`, `community_invite_only_integration.rs`, `community_channel_config_integration.rs`, `community_sync_integration.rs` — these exercise the IPC layer end-to-end via the helper functions. A failure here likely means an HLC ordering assumption that the old "advance only on Inserted" behavior was load-bearing for. Diagnose by reading what the test asserts.
- Tests in `lib.rs::tests` (the mint_*_produces_* trio updated in Task 2) — should pass cleanly given they construct their own HLCs.
- Tests in `dm_outbox.rs::tests` (the three new reservation tests) — should pass.

Common failure case: a test that asserts `tracker.get(device_id) == Some(some_specific_hlc)` after a sequence of operations may now see the tracker hold the LAST-RESERVED HLC instead of the last-INSERTED one. Adjust the assertion: tracker now reflects reserved HLCs, not necessarily inserted ones.

- [ ] **Step 22: Commit**

Run:
```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
refactor(zeb-267): IPC sites use atomic HLC reservation

Refactors all nine reservation sites across eight membership-event
IPCs in lib.rs to call dm_outbox::reserve_next_hlc_for_device:

  - kick_from_community
  - leave_community
  - set_power_level
  - create_channel
  - modify_channel
  - delete_channel
  - create_community_inner (bootstrap_join + default-channel,
    two reservations)
  - redeem_invite_inner

Drops the snapshot-then-release pattern (read tracker, release
lock, mint with stale prev_hlc) and the matching post-Inserted
tracker-advance block at every site. Tracker is now bumped
atomically at reservation time; downstream insert outcome doesn't
matter (HLCs are 64-bit logical, burning on rollback is fine).

create_community_inner gets two reservations in sequence —
bootstrap_join first, then default-channel. The second reservation
reads the just-bumped tracker (== bootstrap_join.at), and next_hlc's
wall-clock regression handling produces a bit-identical HLC to the
prior chained next_hlc(Some(&bootstrap.at), bootstrap.at.wall_ms,
bootstrap.at.device_id) call this replaces.

Stale ZEB-258 comments about deferring tracker advance until after
Space commit are updated in-place.

cargo fmt + clippy + test all green. Test count: Task 0 baseline + 3
(the reservation helper unit tests from Task 1).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 23: Verify the commit landed**

Run: `git log --oneline -4`
Expected: HEAD is the just-created Task 3 commit; Task 2 commit is second; Task 1 third; Task 0 plan fourth.

---

## Task 4: Concurrent-IPC integration test

**Goal:** Add an integration test that exercises two parallel kick-mint-insert flows on the same device against the same engine, asserting both insert successfully with distinct HLCs. This is the test that would have caught the original bug.

**Files:**
- Create: `src-tauri/tests/community_hlc_race_integration.rs`

- [ ] **Step 1: Create the integration test file**

Write to `src-tauri/tests/community_hlc_race_integration.rs`:

```rust
//! ZEB-267: concurrent-IPC HLC race regression test.
//!
//! Stands up a single community on one engine with one admin device
//! (power 100). Spawns two parallel `tokio::join!` futures, each
//! reserving an HLC via `reserve_next_hlc_for_device` then minting +
//! inserting a Kick event for a DIFFERENT target. Asserts:
//!
//!   1. Both engine inserts succeed (`InsertOutcome::Inserted`).
//!   2. The two events' HLCs are distinct under `event_sort_key`
//!      ordering — i.e., the per-device monotone-HLC invariant holds
//!      under concurrent reservation.
//!
//! The pre-ZEB-267 snapshot-then-release pattern would (probabilistically)
//! produce two events with identical HLC tuples, violating the
//! invariant the receive side depends on. With the atomic
//! `reserve_next_hlc_for_device` helper, the race is closed.
//!
//! This test exercises the HELPER + MINT + INSERT path, not the full
//! Tauri IPC boundary. The IPC boundary itself is just a thin wrapper:
//! - hex decode + handle snapshot
//! - the reserve+mint+insert flow this test drives
//! - generation/registry fences that don't interact with HLC ordering
//!
//! Driving the IPC layer directly would require a `tauri::test::mock_app`
//! runtime; the helper-level test is a tighter, faster regression
//! gate that covers the same race surface.

use harmony_app::community_membership::SignedMembershipEvent;
use harmony_app::community_state_crdt::{CommunityState, InsertOutcome};
use harmony_app::community_state_sync::{
    CommunityMembershipDelta, CommunityRootHlcTracker, CommunitySyncEngine,
    CommunitySyncEngineConfig, IdentityResolver, PersistPaths, DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{ContentStore, RuntimeContentStore};
use harmony_app::dm_outbox::reserve_next_hlc_for_device;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use harmony_app::{mint_community_creation, mint_kick_event, mint_set_power_event};
use harmony_identity::PrivateIdentity;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

// Test-only forwarder: when the engine emits CAS ops on `cas_op_rx`,
// service them against an in-memory `HashMap<ContentId, Vec<u8>>`.
enum CasOp {
    PutLocal {
        cid: harmony_content::cid::ContentId,
        blob: Vec<u8>,
        reply: Option<tokio::sync::oneshot::Sender<Result<(), String>>>,
    },
    GetOrFetch {
        cid: harmony_content::cid::ContentId,
        timeout: Duration,
        reply: tokio::sync::oneshot::Sender<Result<Option<Vec<u8>>, String>>,
    },
}

// Implements the in-test IdentityResolver: maps the admin's
// OwnerAddr to its identity_pub. The two kick targets don't need
// resolution because their public keys aren't checked (they're
// not signers — only the actor needs verifiable signing material).
struct AdminOnlyResolver {
    addr: OwnerAddr,
    pubkey: [u8; 64],
}
#[async_trait::async_trait]
impl IdentityResolver for AdminOnlyResolver {
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        if *addr == self.addr {
            Some(self.pubkey)
        } else {
            None
        }
    }
}

fn signing_key_from(identity: &PrivateIdentity) -> Arc<ed25519_dalek::SigningKey> {
    let bytes = identity.to_private_bytes();
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&bytes[32..64]);
    Arc::new(ed25519_dalek::SigningKey::from_bytes(&seed))
}

#[tokio::test]
async fn concurrent_kicks_from_same_device_yield_distinct_hlcs() {
    // ── Setup: admin Alice (power 100), two kick targets Bob/Carol ──
    let alice = PrivateIdentity::from_seed(&[0xa1; 32]);
    let alice_addr = OwnerAddr(alice.identity.address_hash);
    let alice_pub = alice.identity.to_public_bytes();
    let alice_signing = signing_key_from(&alice);
    let bob_addr = OwnerAddr([0xb0; 16]);
    let carol_addr = OwnerAddr([0xc0; 16]);

    let resolver: Arc<dyn IdentityResolver> = Arc::new(AdminOnlyResolver {
        addr: alice_addr,
        pubkey: alice_pub,
    });

    // CAS servicer (in-memory).
    let cas_map: Arc<Mutex<HashMap<harmony_content::cid::ContentId, Vec<u8>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel::<CasOp>(64);
    let cas_for_servicer = Arc::clone(&cas_map);
    tokio::spawn(async move {
        while let Some(op) = cas_op_rx.recv().await {
            match op {
                CasOp::PutLocal { cid, blob, reply } => {
                    cas_for_servicer.lock().await.insert(cid, blob);
                    if let Some(r) = reply {
                        let _ = r.send(Ok(()));
                    }
                }
                CasOp::GetOrFetch {
                    cid,
                    timeout: _,
                    reply,
                } => {
                    let v = cas_for_servicer.lock().await.get(&cid).cloned();
                    let _ = reply.send(Ok(v));
                }
            }
        }
    });

    // Engine pub/sub: not networked — we never expect any publishes
    // to land on `pub_rx` because the test doesn't drive convergence.
    let (pub_tx, _pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (_sub_tx, sub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (delta_tx, _delta_rx) = mpsc::channel::<CommunityMembershipDelta>(32);

    // The IPC-level HLC tracker (per-device monotone HLCs). This is
    // the SAME shape the real IPC uses — `Arc<Mutex<BTreeMap<String, Hlc>>>`.
    let device_id = "alice-dev".to_string();
    let hlc_tracker: Arc<Mutex<BTreeMap<String, Hlc>>> = Arc::new(Mutex::new(BTreeMap::new()));

    // Mint Alice's community + bootstrap Join. Reserve via the helper
    // so the tracker has a valid starting state.
    let bootstrap_hlc = reserve_next_hlc_for_device(&hlc_tracker, &device_id, 100_000).await;
    let minted = mint_community_creation(
        "TestCommunity",
        false, // open
        alice_addr,
        &alice_signing,
        bootstrap_hlc,
    )
    .expect("mint_community_creation");
    let community_id: SpaceId = minted.community_id;

    // Stand up the engine.
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx.clone(),
        Duration::from_secs(2),
    ));
    let state = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let tracker = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));
    let tmp = tempfile::tempdir().expect("tmp");
    let engine = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: minted.membership_key.clone(),
        admin_addr: alice_addr,
        is_invite_only: false,
        device_id: device_id.clone(),
        self_owner: alice_addr,
        signing_key: Arc::clone(&alice_signing),
        state: Arc::clone(&state),
        tracker: Arc::clone(&tracker),
        content_store: cs,
        publisher_tx: pub_tx,
        subscriber_rx: sub_rx,
        paths: PersistPaths {
            crdt: tmp.path().join("crdt.cbor"),
            replay: tmp.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(Arc::clone(&resolver)),
        error_tx: None,
        delta_tx: Some(delta_tx),
        pending_redemptions: None,
    });

    // Insert the bootstrap Join (Alice's self-Join, which gives her
    // power 100 as the admin).
    let outcome = engine
        .insert_local_event(minted.bootstrap_join.clone())
        .await
        .expect("insert bootstrap_join");
    assert_eq!(outcome, InsertOutcome::Inserted, "bootstrap Join must insert");

    // Pre-seed Bob and Carol via SetPower(50) so they're members
    // with non-zero power. mint_set_power_event uses a reserved HLC.
    for target in [bob_addr, carol_addr] {
        let promo_hlc = reserve_next_hlc_for_device(&hlc_tracker, &device_id, 100_000).await;
        let promo = mint_set_power_event(
            community_id,
            alice_addr,
            target,
            50,
            &alice_signing,
            promo_hlc,
        )
        .expect("mint_set_power_event");
        let outcome = engine
            .insert_local_event(promo)
            .await
            .expect("insert SetPower");
        assert_eq!(
            outcome,
            InsertOutcome::Inserted,
            "SetPower(50) must insert for {:?}",
            target
        );
    }

    // ── The actual race test: two concurrent kick reservations ──
    //
    // Both calls happen on the SAME device with the SAME wall_now_ms.
    // Without the atomic helper, the snapshot-then-release pattern
    // would let both observe the same prev_hlc and produce events
    // with identical (wall_ms, logical, device_id) tuples. With
    // reserve_next_hlc_for_device, the read-bump-write is atomic
    // and the two reservations are guaranteed strictly-monotone.
    let wall_now_ms = 200_000u64;
    let tracker_a = Arc::clone(&hlc_tracker);
    let tracker_b = Arc::clone(&hlc_tracker);
    let device_a = device_id.clone();
    let device_b = device_id.clone();
    let signing_a = Arc::clone(&alice_signing);
    let signing_b = Arc::clone(&alice_signing);

    // tokio::join!: both futures run concurrently on the same task,
    // contending for the tracker mutex. The helper serializes them
    // under one read-bump-write critical section each.
    let (kick_bob, kick_carol): (
        Result<SignedMembershipEvent, String>,
        Result<SignedMembershipEvent, String>,
    ) = tokio::join!(
        async {
            let hlc = reserve_next_hlc_for_device(&tracker_a, &device_a, wall_now_ms).await;
            mint_kick_event(
                community_id,
                alice_addr,
                bob_addr,
                Some("race-test bob".into()),
                &signing_a,
                hlc,
            )
        },
        async {
            let hlc = reserve_next_hlc_for_device(&tracker_b, &device_b, wall_now_ms).await;
            mint_kick_event(
                community_id,
                alice_addr,
                carol_addr,
                Some("race-test carol".into()),
                &signing_b,
                hlc,
            )
        },
    );

    let kick_bob = kick_bob.expect("mint kick(bob)");
    let kick_carol = kick_carol.expect("mint kick(carol)");

    // ── Assertion 1: HLCs distinct under sort-key ordering ─────────
    let bob_key = (kick_bob.at.wall_ms, kick_bob.at.logical, &kick_bob.at.device_id);
    let carol_key = (
        kick_carol.at.wall_ms,
        kick_carol.at.logical,
        &kick_carol.at.device_id,
    );
    assert_ne!(
        bob_key, carol_key,
        "concurrent reservations must produce distinct sort keys; \
         got bob={:?} carol={:?}",
        kick_bob.at, kick_carol.at
    );

    // ── Assertion 2: both engine inserts succeed ────────────────────
    let outcome_bob = engine
        .insert_local_event(kick_bob.clone())
        .await
        .expect("insert kick(bob)");
    let outcome_carol = engine
        .insert_local_event(kick_carol.clone())
        .await
        .expect("insert kick(carol)");
    assert_eq!(outcome_bob, InsertOutcome::Inserted, "kick(bob) must insert");
    assert_eq!(
        outcome_carol,
        InsertOutcome::Inserted,
        "kick(carol) must insert"
    );

    // ── Assertion 3: tracker holds the LATER of the two HLCs ───────
    let stored = hlc_tracker
        .lock()
        .await
        .get(&device_id)
        .cloned()
        .expect("tracker entry");
    let max_kick_at = if (kick_bob.at.wall_ms, kick_bob.at.logical, &kick_bob.at.device_id)
        > (
            kick_carol.at.wall_ms,
            kick_carol.at.logical,
            &kick_carol.at.device_id,
        ) {
        kick_bob.at
    } else {
        kick_carol.at
    };
    assert_eq!(
        stored, max_kick_at,
        "tracker must hold the max-by-sort-key HLC of the two reservations"
    );

    engine.shutdown().await.expect("shutdown");
}
```

- [ ] **Step 2: Confirm `reserve_next_hlc_for_device` is re-exported from `harmony_app::dm_outbox`**

Run: `grep -n "pub use\|pub mod dm_outbox\|reserve_next_hlc_for_device" src-tauri/src/lib.rs | head -5`
Expected: a line like `pub mod dm_outbox;` showing dm_outbox is a public module. That means `harmony_app::dm_outbox::reserve_next_hlc_for_device` resolves correctly.

If `dm_outbox` is `pub(crate)` instead of `pub`, the integration test won't link. In that case, change `pub(crate) fn` to `pub fn` on the helper definition (the integration tests at `src-tauri/tests/` are external consumers of the crate — they need `pub` visibility).

Confirm by running `grep -n "pub.*fn next_hlc\|pub.*fn reserve_next_hlc_for_device" src-tauri/src/dm_outbox.rs`. The existing `next_hlc` is `pub(crate)`; `reserve_next_hlc_for_device` from Task 1 was added as `pub`. If both need to be `pub` for the integration test to use them, no change is needed — the test only uses `reserve_next_hlc_for_device`. If `mint_kick_event` etc. need to be re-exported from `harmony_app::*`, check `lib.rs` — they're used in `community_sync_integration.rs` already so the re-exports are there.

- [ ] **Step 3: Run the new integration test**

Run: `cd src-tauri && cargo test --test community_hlc_race_integration concurrent_kicks_from_same_device_yield_distinct_hlcs 2>&1 | tee /tmp/zeb267-task4-test.log; echo "test exit: ${PIPESTATUS[0]}"`
Expected: final line `test exit: 0`; output contains `running 1 test` and `1 passed`.

- [ ] **Step 4: (Optional TDD verification — do NOT commit) confirm the test would have caught the bug**

Stash Task 3's call-site changes:
```bash
git stash push -m "zeb267-task3-stash" -- src-tauri/src/lib.rs
```

Run the test:
```bash
cd src-tauri && cargo test --test community_hlc_race_integration concurrent_kicks_from_same_device_yield_distinct_hlcs 2>&1 | tail -30
```

Expected: NON-zero exit OR a probabilistic failure showing colliding HLCs. (The race is timing-dependent; in CI it might still pass. The point of this step is to confirm the test EXERCISES the race surface, not necessarily to deterministically reproduce the bug.) Note this step is optional — if the test passes both pre- and post-fix, the failure mode is too fast to reproduce without injected delay; the unit tests in Task 1 (Step 2's `reserve_next_hlc_for_device_concurrent_reservations_distinct`) provide the deterministic guarantee.

Restore Task 3's changes:
```bash
git stash pop
```

Re-run the test:
```bash
cd src-tauri && cargo test --test community_hlc_race_integration concurrent_kicks_from_same_device_yield_distinct_hlcs 2>&1 | tail -30
```

Expected: PASS, returning to the post-Task-3 state.

- [ ] **Step 5: Run the full workspace test suite**

Run: `cd src-tauri && cargo test --workspace --no-fail-fast 2>&1 | tee /tmp/zeb267-task4-full.log; echo "test exit: ${PIPESTATUS[0]}"`
Expected: final line `test exit: 0`. Test count is now Task 0 baseline + 3 (Task 1 unit tests) + 1 (Task 4 integration test).

- [ ] **Step 6: Run cargo fmt + clippy**

Run: `cd src-tauri && cargo fmt --all && cargo clippy --all-targets -- -D warnings`
Expected: both exit 0.

- [ ] **Step 7: Commit**

Run:
```bash
git add src-tauri/tests/community_hlc_race_integration.rs
git commit -m "$(cat <<'EOF'
test(zeb-267): concurrent-IPC HLC race regression test

New integration test in tests/community_hlc_race_integration.rs.
Drives two concurrent kick mints (different targets) against a
single engine with one admin device, asserting both insert
successfully and produce distinct HLCs under event_sort_key
ordering. Pre-ZEB-267 the snapshot-then-release pattern would
(probabilistically) produce two events with identical HLC tuples,
violating the per-device monotone-HLC invariant; with the atomic
reserve_next_hlc_for_device helper, the race is closed.

Test exercises the helper + mint + insert path directly; the IPC
boundary itself is just a thin wrapper (hex decode + handle
snapshot + generation/registry fences) that doesn't interact with
HLC ordering. Driving via tauri::test::mock_app would duplicate
that scaffolding without covering additional race surface.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 8: Verify the commit landed**

Run: `git log --oneline -5`
Expected: HEAD is the just-created Task 4 commit; Tasks 3, 2, 1, 0 follow.

---

## Task 5: Final verification + push + PR

**Goal:** Re-run all gates green, push the branch to GitHub, open a PR cross-referencing the spec, plan, and CodeRabbit thread.

**Files:**
- Read-only: `src-tauri/src/dm_outbox.rs`, `src-tauri/src/lib.rs`, `src-tauri/tests/community_hlc_race_integration.rs`, `docs/specs/2026-05-09-zeb-267-atomic-hlc-reservation-design.md`, `docs/plans/2026-05-09-zeb-267-atomic-hlc-reservation-plan.md`

- [ ] **Step 1: Final cargo fmt check**

Run: `cd src-tauri && cargo fmt --all -- --check`
Expected: exit 0, no output.

- [ ] **Step 2: Final cargo clippy check**

Run: `cd src-tauri && cargo clippy --all-targets -- -D warnings`
Expected: exit 0.

- [ ] **Step 3: Final cargo test run**

Run: `cd src-tauri && cargo test --workspace --no-fail-fast 2>&1 | tee /tmp/zeb267-final.log; echo "test exit: ${PIPESTATUS[0]}"`
Expected: final line `test exit: 0`. Test count: Task 0 baseline + 4 (3 helper unit + 1 integration). Capture the line `test result: ok. N passed; 0 failed; M ignored` for the PR body.

- [ ] **Step 4: Re-confirm branch state and commit chain**

Run: `git status --short && git log --oneline origin/main..HEAD`
Expected:
- Status output is empty (clean working tree).
- Six commits visible since `b67468f` `origin/main`:
  1. `docs(zeb-267): atomic HLC reservation design spec` (committed at `70d7a99` before implementation)
  2. `docs(zeb-267): implementation plan for atomic HLC reservation refactor` (committed before implementation)
  3. `feat(zeb-267): atomic reserve_next_hlc_for_device helper` (Task 1)
  4. `refactor(zeb-267): mint_* helpers take pre-reserved Hlc directly` (Task 2)
  5. `refactor(zeb-267): IPC sites use atomic HLC reservation` (Task 3)
  6. `test(zeb-267): concurrent-IPC HLC race regression test` (Task 4)

If git log shows fewer commits or a different shape, audit the previous tasks and reconcile before pushing.

- [ ] **Step 5: Push the branch to GitHub**

Run: `git push -u origin zeb-267-atomic-hlc-reservation`
Expected: branch pushed, GitHub returns the URL.

- [ ] **Step 6: Open the PR**

Run:
```bash
gh pr create --title "ZEB-267: atomic HLC reservation for device-monotone event minting" --body "$(cat <<'EOF'
## Summary

Closes the snapshot-then-release HLC race in nine reservation sites across eight power-gated community-event IPCs, surfaced by CodeRabbit on PR #93 (ZEB-266 Sub-C v2 Phase 1) and deferred there as cross-cutting tech debt. Implements the `reserve_next_hlc_for_device` helper plan CodeRabbit accepted.

* Adds `reserve_next_hlc_for_device` helper in `dm_outbox.rs` — atomic read-bump-write of the per-device HLC tracker under one lock acquisition.
* Simplifies eight `mint_*` membership-event helpers in `lib.rs` to take a pre-reserved `Hlc` directly. Drops their `(wall_now_ms, device_id, prev_hlc)` parameter trio — mint helpers are now pure on the HLC.
* Refactors nine reservation sites across eight IPCs (`kick_from_community`, `leave_community`, `set_power_level`, `create_channel`, `modify_channel`, `delete_channel`, `create_community_inner` × 2, `redeem_invite_inner`) to call the helper and remove the post-Inserted tracker-advance.
* Adds three unit tests on the helper (sequential, 64 concurrent, wall-clock regression) and one integration test exercising two concurrent kick mints from the same device.

## Why now

Phase 1 of ZEB-248 (PR #93) added four new IPCs (`create_channel`, `modify_channel`, `delete_channel`, plus the default-channel mint inside `create_community_inner`) on top of the existing five power-gated mint sites. Doing this refactor BEFORE Phase 2 (ChannelLog data plane) means Phase 2's new IPCs inherit the cleaner pattern from day one rather than adding more sites to an already race-prone surface and refactoring 11+ sites later.

## Architecture & decisions

See spec at `docs/specs/2026-05-09-zeb-267-atomic-hlc-reservation-design.md` (commit 70d7a99) for the full design. Key choices:

* **Helper colocated with `next_hlc`** in `dm_outbox.rs` — consistent with the existing comment at line 1521 about deferring shared-module promotion.
* **Mint helpers take `Hlc` directly** (not `prev_hlc` + recomputed `next_hlc`) — cleaner abstraction, smaller signatures. The `(wall_now_ms, device_id, prev_hlc)` trio collapses to a single `hlc: Hlc` arg.
* **Tracker bumped at reservation time**, not post-Inserted — simpler reasoning, atomic guarantee. "Burn semantics": a rejected insert leaves the reserved HLC unused; HLCs are 64-bit logical so burning is fine.
* **`create_community_inner` two-event case** uses two sequential reservations (no special `reserve_n` API) — the second reads the just-bumped tracker and returns a strictly-greater HLC. `next_hlc`'s wall-clock regression handling produces a bit-identical result to the prior chained `next_hlc(Some(&bootstrap.at), ...)` call.

## Out of scope (per spec §4)

* `send_dm` and `DmOutbox::send_dm` — already race-free under tracker lock (lines 2188-2218).
* Receive-side `next_hlc` callers in `owner_state_sync.rs:428` / `community_state_sync.rs:1291` — already serialized by their engine locks.
* Promoting `next_hlc` to a shared module.
* Wrapping the tracker in a `HlcTracker` newtype.

## Verification

* `cargo fmt --all -- --check` — green
* `cargo clippy --all-targets -- -D warnings` — green
* `cargo test --workspace --no-fail-fast` — green; test count = baseline + 4 (3 reservation unit tests + 1 race integration test)

## Test plan

- [ ] CI green: Rust fmt + clippy + test
- [ ] CI green: MSRV
- [ ] CI green: Frontend tsc + vitest
- [ ] CodeRabbit review pass with no Major findings
- [ ] Cursor Bugbot review pass with no HIGH findings
- [ ] mergeStateStatus CLEAN

## References

* Linear: ZEB-267 (https://linear.app/zeblith/issue/ZEB-267)
* Predecessor: ZEB-266 (Sub-C v2 Phase 1) — surfaced the bug via PR #93's CodeRabbit thread (round 1, Major)
* Parent: ZEB-217 (Sub-C v1) — established the device-HLC tracker pattern this refactor cleans up

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Capture the PR URL from the gh output.

- [ ] **Step 7: Verify the PR opened**

Run: `gh pr view --json number,title,url,state`
Expected: a JSON object with `state: "OPEN"`, the title from Step 6, and a numerical PR number. Capture the URL.

- [ ] **Step 8: Report convergence to the user**

Provide the PR URL and the final commit chain summary. Do NOT poll for CI completion in this step — the user's standard practice is to monitor PR convergence in a separate loop, signaled by a follow-up message.

---

## Plan complete — execution handoff

Plan saved to `docs/plans/2026-05-09-zeb-267-atomic-hlc-reservation-plan.md`. Two execution options:

1. **Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.
2. **Inline Execution** — Execute tasks in this session using `superpowers:executing-plans`, batch execution with checkpoints.

Which approach?
