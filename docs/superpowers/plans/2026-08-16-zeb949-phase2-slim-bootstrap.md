# ZEB-949 Phase 2 — Slim-Bootstrap Community Invites — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop inlining the O(members) roster snapshot in community invites so an invite fits a plain Discord message for a community of any size; the roster syncs P2P after redemption.

**Architecture:** One production change — `generate_invite_impl` emits an empty `MaterializedCommunityState` instead of the materialized roster. The snapshot has always been a documented "UI bootstrap hint" (ZEB-249), never consulted by the membership-at-HLC gate (proven by the ZEB-947 spike), so decode/redeem/verification are unchanged. The frontend gains a graceful "initial sync" state so the ~1s empty window isn't misleading.

**Tech Stack:** Rust (`src-tauri/`, canonical CBOR + `flate2` deflate from Phase 1), Svelte 5 + TypeScript frontend (`src/`), `cargo nextest`, `vitest`.

**Spec:** `docs/superpowers/specs/2026-08-16-zeb947-phase2-slim-bootstrap-design.md` (read it alongside this plan).

## Global Constraints

- **Uniform slim, drop-everything:** the invite's `state_snapshot` is ALWAYS `MaterializedCommunityState::default()` (empty members, channels, power_levels). No adaptive policy.
- **No wire-format change:** the `state_snapshot` field type stays `MaterializedCommunityState`; its maps already serialize empty. No `Option`, no struct change, no version byte.
- **`pre_fork_snapshot` is NOT slimmed** by this phase (separate fork-seeding question; the existing oversize-fork fallback stays the safety net).
- **Rust gates (run from `src-tauri/`):** `cargo nextest run --locked --workspace --all-targets --features test-fixtures`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo fmt --all -- --check`. A lib change relinks ~97 binaries — during iteration use `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(<name>)'`.
- **Frontend gates (run from repo root):** `npx tsc --noEmit`; `npx vitest run`.
- **Commit discipline:** one PR for the whole ZEB-949 change on branch `zeblith/zeb-949-...`; do not merge (operator-only).

---

### Task 1: Membership-gate regression tests (roster-less bootstrap invariant)

Pins the load-bearing Phase-2 invariant proven by the spike: a joiner with no inlined roster verifies inbound events from the synced log alone, and the gate's verify-on-insert / delivery-order-recovery behavior holds. These characterize existing behavior Phase 2 depends on — they pass against current code and guard against a future regression that would couple verification to the roster.

**Files:**
- Create: `src-tauri/src/community_invite_slim_bootstrap_tests.rs`
- Modify: `src-tauri/src/lib.rs` (add one `#[cfg(test)] mod` declaration next to `mod simnet;` at `lib.rs:344-346`)

**Interfaces:**
- Consumes (all crate-internal, reachable from an in-crate `#[cfg(test)]` module): `crate::community_membership::{materialize, mint_test_owner, MemberStatus, VerifyContext}`, `crate::community_state_crdt::{CommunityState, InsertOutcome}`, `crate::community_invite::{CommunityInvitePayload, InviteEpochSnapshot, MaterializedCommunityState}`, `crate::owner_state_types::{Hlc, OwnerAddr, SpaceId}`, and crate-root `crate::{mint_community_creation, mint_redemption, mint_leave_event}`.
- Produces: nothing consumed by later tasks (pure test coverage).

- [ ] **Step 1: Create the test module file**

Create `src-tauri/src/community_invite_slim_bootstrap_tests.rs`:

```rust
//! ZEB-949 Phase 2 — regression coverage for slim-bootstrap invites.
//!
//! Proves the receive-side membership-at-HLC gate needs only `admin_bootstrap`
//! + the P2P-synced event log — never the inlined roster snapshot. Exercises the
//! real gate (`CommunityState::insert_event` -> `verify_event` against the
//! strictly-prior materialized state). Also pins the size property of a slim
//! invite (Task 2 appends there).
#![cfg(test)]

use std::collections::{BTreeMap, BTreeSet};

use crate::community_invite::{
    encode_invite_url, CommunityInvitePayload, InviteEpochSnapshot, MaterializedCommunityState,
};
use crate::community_membership::{
    materialize, mint_test_owner, MemberState, MemberStatus, VerifyContext,
};
use crate::community_state_crdt::{CommunityState, InsertOutcome};
use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};

/// An OPEN invite whose inlined roster snapshot is EMPTY — the Phase-2 slim shape.
fn slim_open_invite(
    community_id: SpaceId,
    admin_addr: OwnerAddr,
    membership_key_bytes: Vec<u8>,
) -> CommunityInvitePayload {
    CommunityInvitePayload {
        inviter_signer_certs: Vec::new(),
        community_id,
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: membership_key_bytes,
            sealed_epoch_keys: Vec::new(),
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr,
        community_name: "SlimComm".into(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        admin_identity_pub: None,
        forked_from: None,
        pre_fork_snapshot: None,
        inviter_enrollment: None,
        untargeted_decrypt_key: None,
    }
}

fn hlc(wall_ms: u64, device_id: &str) -> Hlc {
    Hlc { wall_ms, logical: 0, device_id: device_id.to_string() }
}

/// A fresh empty-log joiner (no roster) accepts a member-authored GATED event
/// and materializes the full roster — from the synced log alone.
#[test]
fn slim_bootstrap_joiner_verifies_full_community_from_synced_log_alone() {
    let admin = mint_test_owner(1);
    let minted_admin = crate::mint_community_creation(
        "SlimComm", false, admin.owner, &admin.device_key, &admin.cert,
        hlc(100_000, "admin-dev"),
    )
    .expect("mint create");
    let community_id = minted_admin.community_id;
    let membership_key = minted_admin.membership_key.clone();

    let bob = mint_test_owner(2);
    let invite = slim_open_invite(community_id, admin.owner, membership_key.as_bytes().to_vec());
    let minted_bob = crate::mint_redemption(
        &invite, bob.owner, &bob.device_key, &bob.cert, hlc(200_000, "bob-dev"),
    )
    .expect("mint redeem");

    let bob_leave = crate::mint_leave_event(
        community_id, bob.owner, &bob.device_key, hlc(300_000, "bob-dev"),
    )
    .expect("mint leave");

    let mut joiner = CommunityState::new(community_id);
    let ctx = VerifyContext {
        expected_community_id: community_id,
        admin_addr: admin.owner,
        is_invite_only: false,
        now_ms: None,
    };

    for (label, ev) in [
        ("admin bootstrap Join", minted_admin.bootstrap_join.clone()),
        ("Bob redemption Join", minted_bob.bootstrap_join.clone()),
        ("Bob-authored Leave (gated)", bob_leave.clone()),
    ] {
        let outcome = joiner.insert_event(ev, &ctx);
        assert!(
            matches!(outcome, InsertOutcome::Inserted),
            "roster-less joiner's gate rejected {label}: {outcome:?}"
        );
    }

    let events: Vec<_> = joiner.events().cloned().collect();
    assert_eq!(events.len(), 3);
    let mat = materialize(&events, admin.owner);
    assert_eq!(mat.members.get(&admin.owner).map(|m| m.status), Some(MemberStatus::Joined));
    assert_eq!(
        mat.members.get(&bob.owner).map(|m| m.status),
        Some(MemberStatus::Left),
        "Bob's gated Leave verified + applied — the gate needed no inlined roster"
    );
}

/// The gate is verify-ON-INSERT: an out-of-order member event is Rejected and
/// recovers on re-delivery once the Join lands. This is why Phase-2 sync must
/// deliver Join-before-authored (a sort-ordered state-root batch merge does) and
/// why the engine defers-not-drops (ZEB-526) unknown publishers.
#[test]
fn gate_is_on_insert_out_of_order_member_event_rejected_then_recovers() {
    let admin = mint_test_owner(1);
    let minted_admin = crate::mint_community_creation(
        "SlimComm", false, admin.owner, &admin.device_key, &admin.cert,
        hlc(100_000, "admin-dev"),
    )
    .expect("mint create");
    let community_id = minted_admin.community_id;
    let membership_key = minted_admin.membership_key.clone();

    let bob = mint_test_owner(2);
    let invite = slim_open_invite(community_id, admin.owner, membership_key.as_bytes().to_vec());
    let minted_bob = crate::mint_redemption(
        &invite, bob.owner, &bob.device_key, &bob.cert, hlc(200_000, "bob-dev"),
    )
    .expect("mint redeem");
    let bob_leave = crate::mint_leave_event(
        community_id, bob.owner, &bob.device_key, hlc(300_000, "bob-dev"),
    )
    .expect("mint leave");

    let mut joiner = CommunityState::new(community_id);
    let ctx = VerifyContext {
        expected_community_id: community_id,
        admin_addr: admin.owner,
        is_invite_only: false,
        now_ms: None,
    };

    assert!(matches!(
        joiner.insert_event(minted_admin.bootstrap_join.clone(), &ctx),
        InsertOutcome::Inserted
    ));

    let early = joiner.insert_event(bob_leave.clone(), &ctx);
    assert!(
        matches!(early, InsertOutcome::Rejected(_)),
        "out-of-order member event must be rejected by the on-insert gate, got {early:?}"
    );

    assert!(matches!(
        joiner.insert_event(minted_bob.bootstrap_join.clone(), &ctx),
        InsertOutcome::Inserted
    ));

    {
        let events: Vec<_> = joiner.events().cloned().collect();
        let mat = materialize(&events, admin.owner);
        assert_eq!(mat.members.get(&bob.owner).map(|m| m.status), Some(MemberStatus::Joined));
    }

    assert!(matches!(
        joiner.insert_event(bob_leave.clone(), &ctx),
        InsertOutcome::Inserted
    ));
    let events: Vec<_> = joiner.events().cloned().collect();
    let mat = materialize(&events, admin.owner);
    assert_eq!(mat.members.get(&bob.owner).map(|m| m.status), Some(MemberStatus::Left));
}
```

- [ ] **Step 2: Declare the module in lib.rs**

At `src-tauri/src/lib.rs:344-346`, immediately after the `#[cfg(test)] mod simnet;` block, add:

```rust
// ZEB-949 Phase 2: slim-bootstrap invite regression + size coverage.
#[cfg(test)]
mod community_invite_slim_bootstrap_tests;
```

- [ ] **Step 3: Run the two tests — expect PASS (characterization of existing behavior)**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(slim_bootstrap_joiner) + test(gate_is_on_insert)'`
Expected: 2 passed. (These pin behavior that already holds — the spike proved it.)

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/community_invite_slim_bootstrap_tests.rs src-tauri/src/lib.rs
git commit -m "test(invite): ZEB-949 roster-less bootstrap gate regression tests"
```

---

### Task 2: Codec size fixture (O(1) proof + old-shape-blows-cap)

The one test that asserts the asymptotic property with concrete numbers: a slim (empty-snapshot) invite fits Discord's 2000-char cap, while the old full-roster shape at N=500 exceeds it. Guards against a future refactor silently re-inlining the roster.

**Files:**
- Modify: `src-tauri/src/community_invite_slim_bootstrap_tests.rs` (append)

**Interfaces:**
- Consumes: `encode_invite_url`, `MaterializedCommunityState`, `MemberState`, `MemberStatus`, `OwnerAddr`, `SpaceId`, `Hlc`, and the `slim_open_invite`/`hlc` helpers from Task 1 (same module).
- Produces: nothing.

- [ ] **Step 1: Append the size test**

Append to `src-tauri/src/community_invite_slim_bootstrap_tests.rs`:

```rust
/// Build a synthetic MaterializedCommunityState with `n` members carrying
/// pseudo-random (incompressible) device-key + owner-addr bytes, so the size
/// measurement reflects the real cryptographic-core cost per member.
fn synthetic_roster(n: usize) -> MaterializedCommunityState {
    let mut members = BTreeMap::new();
    for i in 0..n {
        // Cheap deterministic spread across all bytes (no rng dependency).
        let mut addr = [0u8; 16];
        for (j, b) in addr.iter_mut().enumerate() {
            *b = i.wrapping_mul(2_654_435_761).wrapping_add(j.wrapping_mul(97)) as u8;
        }
        let mut key = [0u8; 32];
        for (j, b) in key.iter_mut().enumerate() {
            *b = i.wrapping_mul(40_503).wrapping_add(j.wrapping_mul(131)).wrapping_add(7) as u8;
        }
        let mut keys = BTreeSet::new();
        keys.insert(key);
        members.insert(
            OwnerAddr(addr),
            MemberState {
                status: MemberStatus::Joined,
                joined_at: Hlc { wall_ms: 100 + i as u64, logical: 0, device_id: "d".into() },
                left_at: None,
                enrolled_device_keys: keys,
                revoked_device_keys: BTreeSet::new(),
            },
        );
    }
    MaterializedCommunityState { members, channels: BTreeMap::new(), power_levels: BTreeMap::new() }
}

/// A payload with a given snapshot; everything else fixed and minimal.
fn payload_with_snapshot(snapshot: MaterializedCommunityState) -> CommunityInvitePayload {
    let mut p = slim_open_invite(SpaceId([7u8; 16]), OwnerAddr([1u8; 16]), vec![0u8; 32]);
    p.epoch_snapshot.state_snapshot = snapshot;
    p
}

#[test]
fn slim_invite_fits_cap_while_old_full_roster_blows_it() {
    // Slim (empty snapshot): under Discord's 2000-char cap.
    let slim = encode_invite_url(&payload_with_snapshot(MaterializedCommunityState::default()))
        .expect("encode slim");
    assert!(slim.len() < 2000, "slim invite must fit the 2000-char cap: {} chars", slim.len());

    // Old full-roster shape at N=500: exceeds the cap (the regression Phase 2 fixes).
    let full = encode_invite_url(&payload_with_snapshot(synthetic_roster(500)))
        .expect("encode full");
    assert!(full.len() > 2000, "old full-roster N=500 should exceed the cap: {} chars", full.len());

    // Slim size is content-independent (the roster is simply not present).
    let slim_again = encode_invite_url(&payload_with_snapshot(MaterializedCommunityState::default()))
        .expect("encode slim again");
    assert_eq!(slim.len(), slim_again.len(), "slim size does not depend on community size");
}
```

- [ ] **Step 2: Run — expect PASS**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(slim_invite_fits_cap)'`
Expected: 1 passed. If `full.len()` is not > 2000, the synthetic per-member bytes are too compressible — raise N or increase per-member key entropy; do not lower the assertion.

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/community_invite_slim_bootstrap_tests.rs
git commit -m "test(invite): ZEB-949 codec size fixture — slim O(1) vs full-roster over cap"
```

---

### Task 3: Encoder change — emit an empty snapshot

The single production change: `generate_invite_impl` stops materializing and inlining the roster. Guarded by the existing generate/redeem integration suite (which already constructs and redeems empty-snapshot invites) plus Tasks 1–2. Per repo convention (`community_fork_integration.rs:748` — "The full generate_invite → redeem_invite_inner path requires Tauri"), `generate_invite_impl` is not directly harnessed; the behavioral guards live at the codec/gate/redeem layers.

**Files:**
- Modify: `src-tauri/src/lib.rs:36187-36207` (the `let state_snapshot = { ... };` block in `generate_invite_impl`)

**Interfaces:**
- Consumes: `crate::community_invite::MaterializedCommunityState` (already in scope in `lib.rs`).
- Produces: `generate_invite_impl` now emits `epoch_snapshot.state_snapshot == MaterializedCommunityState::default()`.

- [ ] **Step 1: Task 0 — verify no hard roster dependency in the redeem path**

Read `redeem_invite_inner` and the `seed_bootstrap_hint` consumers (start at `lib.rs:41919-41940`), and confirm the snapshot is only used to seed the UI hint, never asserted non-empty. Corroborating evidence already in-tree (no new code): `tests/community_sync/community_open_flow_integration.rs` and `tests/community_misc/community_invite_only_integration.rs` construct `state_snapshot: MaterializedCommunityState::default()` invites and redeem them end-to-end today. Record the confirmation in the task notes.
Expected outcome: no hard dependency (the ZEB-249 contract). If — unexpectedly — one exists, STOP and escalate; the fallback is a version-tag gate on emission (out of scope unless triggered).

- [ ] **Step 2: Replace the snapshot-building block**

In `src-tauri/src/lib.rs`, replace the entire block at `36187-36207`:

```rust
    let state_snapshot = {
        let materialized = if engine_state.is_some() {
            // R4-6: pass wall_now_ms so an idle-community PendingJoin
            // already past 30d is excluded from the bootstrap snapshot
            // sent to a new invitee.
            let wall_now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            crate::community_membership::materialize_with_now(&events, admin, Some(wall_now_ms))
        } else {
            // No engine yet (e.g., just-created community with no events):
            // fall back to empty maps — still a valid bootstrap hint.
            crate::community_membership::MaterializedMembership::default()
        };
        crate::community_invite::MaterializedCommunityState {
            members: materialized.members,
            channels: materialized.channels,
            power_levels: materialized.power_levels,
        }
    };
```

with:

```rust
    // ZEB-949 Phase 2: invites no longer inline the roster — members, channels,
    // and policies sync P2P after redemption (spike verdict: the snapshot is a
    // UI hint, never consulted by the membership-at-HLC gate). O(members) -> O(1).
    // This also drops, for free, the zeroed ed25519_pub / PQ placeholder fields
    // that lived per-member inside the roster.
    let state_snapshot = crate::community_invite::MaterializedCommunityState::default();
```

If the now-removed block leaves `events` unused elsewhere in the function, the compiler/clippy will flag it; remove the now-dead `events` binding (search upward from `36187` for `let events` / `let ... events =`) only if it has no other consumer. Do NOT remove it if anything below still reads it.

- [ ] **Step 3: Verify the change compiles and existing generate/redeem tests stay green**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(invite) + test(redeem) + test(community_open_flow) + test(community_fork)'`
Expected: all green (the real function still generates; redeem still works with the empty snapshot). Then confirm clippy is clean for the touched file:
Run: `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: no warnings (in particular, no `unused variable: events`).

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(invite): ZEB-949 emit empty state_snapshot (slim bootstrap)"
```

---

### Task 4: Frontend initial-sync UX (relabel the misleading empty window)

With the roster no longer inlined, a freshly-joined community is empty for ~1s until sync. Add a per-community `initialSyncing` signal and relabel the member panel and channel empty-state so the window reads as "arriving," mirroring the existing `channelSyncing` idiom (`ChannelMessageFeed.svelte:827-833`). Members/channels already update reactively (`community-members-changed` / `channel-config-updated`), so no data-flow change is needed — only the empty-state copy.

**Files:**
- Modify: `src/App.svelte` (redeem handler `~4722-4765`; `onMembersChanged` wiring `~2067-2076`; add `initialSyncing` state + timeout)
- Modify: `src/lib/components/ChannelMembersPanel.svelte` (member empty state `~126-168`)
- Modify: `src/lib/components/CommunityView.svelte` (channel empty state `~547-553`)
- Create: `src/lib/community-initial-sync.ts` (a small, unit-testable store for the flag)
- Test: `src/lib/community-initial-sync.test.ts`

**Interfaces:**
- Produces: `createInitialSyncTracker()` returning `{ markJoined(communityId: string): void, clear(communityId: string): void, isSyncing(communityId: string): boolean }`, with an internal ~10s timeout auto-clear. `App.svelte` owns one instance and threads `isSyncing(selectedCommunityId)` into `CommunityView` → `ChannelMembersPanel`.

- [ ] **Step 1: Write the failing store test**

Create `src/lib/community-initial-sync.test.ts`:

```ts
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { createInitialSyncTracker } from './community-initial-sync';

describe('initial-sync tracker', () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it('reports syncing after markJoined and stops after clear', () => {
    const t = createInitialSyncTracker();
    expect(t.isSyncing('c1')).toBe(false);
    t.markJoined('c1');
    expect(t.isSyncing('c1')).toBe(true);
    t.clear('c1');
    expect(t.isSyncing('c1')).toBe(false);
  });

  it('auto-clears after the timeout safety-valve', () => {
    const t = createInitialSyncTracker(10_000);
    t.markJoined('c1');
    expect(t.isSyncing('c1')).toBe(true);
    vi.advanceTimersByTime(10_000);
    expect(t.isSyncing('c1')).toBe(false);
  });

  it('tracks communities independently', () => {
    const t = createInitialSyncTracker();
    t.markJoined('c1');
    expect(t.isSyncing('c1')).toBe(true);
    expect(t.isSyncing('c2')).toBe(false);
  });
});
```

- [ ] **Step 2: Run it — expect FAIL (module missing)**

Run: `npx vitest run src/lib/community-initial-sync.test.ts`
Expected: FAIL — cannot resolve `./community-initial-sync`.

- [ ] **Step 3: Implement the store**

Create `src/lib/community-initial-sync.ts`:

```ts
/**
 * ZEB-949 Phase 2: tracks which freshly-joined communities are still doing
 * their first roster/channel sync, so the UI can show "Syncing…" instead of a
 * misleading empty state. Transient/in-memory: cleared on first synced content
 * (caller) or by a timeout safety-valve.
 */
export function createInitialSyncTracker(timeoutMs = 10_000) {
  const syncing = new Set<string>();
  const timers = new Map<string, ReturnType<typeof setTimeout>>();

  function clear(communityId: string): void {
    syncing.delete(communityId);
    const t = timers.get(communityId);
    if (t !== undefined) {
      clearTimeout(t);
      timers.delete(communityId);
    }
  }

  function markJoined(communityId: string): void {
    syncing.add(communityId);
    const existing = timers.get(communityId);
    if (existing !== undefined) clearTimeout(existing);
    timers.set(communityId, setTimeout(() => clear(communityId), timeoutMs));
  }

  function isSyncing(communityId: string): boolean {
    return syncing.has(communityId);
  }

  return { markJoined, clear, isSyncing };
}
```

- [ ] **Step 4: Run the test — expect PASS**

Run: `npx vitest run src/lib/community-initial-sync.test.ts`
Expected: 3 passed.

- [ ] **Step 5: Wire the tracker into App.svelte**

In `src/App.svelte`: instantiate one tracker near the other module state, e.g. `const initialSync = createInitialSyncTracker();` (add `import { createInitialSyncTracker } from './lib/community-initial-sync';`). In the redeem-success handler (`~4722-4765`), after `changeSelectedCommunity(dto.communityId)`, call `initialSync.markJoined(dto.communityId);`. In `onMembersChanged` (`~2067-2076`), after a refresh that yields more than just self, call `initialSync.clear(communityId);` — i.e. clear once real members arrive. Do the same in the channel-config-updated handler (`~2406-2407`) once `channels.length > 0`. Expose a reactive `$derived` boolean for the selected community, e.g. `const communityInitialSyncing = $derived(selectedCommunityId ? initialSync.isSyncing(selectedCommunityId) : false);`, and pass it down to `CommunityView` as a prop (`initialSyncing={communityInitialSyncing}`). Note: because `isSyncing` reads a plain `Set`, wrap the tracker's backing state so Svelte re-derives — simplest is to keep a `$state` counter bumped on `markJoined`/`clear` and reference it inside the `$derived`; follow the surrounding App.svelte reactivity conventions.

- [ ] **Step 6: Relabel the member panel empty state**

In `src/lib/components/ChannelMembersPanel.svelte`, add an `initialSyncing` prop (thread it CommunityView → ChannelMembersPanel). At the empty-list region (`~126-168`), when `initialSyncing && visible.length <= 1` (only self, or none) and not already in the IPC `loading` state, render a syncing placeholder mirroring `ChannelMessageFeed.svelte:827-833`, e.g. a list item / muted line reading "Syncing members…". Keep the existing `loading` ("Loading members…") branch untouched — `initialSyncing` is the distinct CRDT-sync-window signal.

- [ ] **Step 7: Relabel the channel empty state**

In `src/lib/components/CommunityView.svelte`, add/thread an `initialSyncing` prop. At the channel empty state (`~547-553`), when `initialSyncing` is true, render "Syncing channels…" instead of "No channels in this community yet." When `initialSyncing` is false, keep the existing message (and the "Create channel" affordance for `myPower >= 50`).

- [ ] **Step 8: Frontend gates**

Run: `npx tsc --noEmit` (expect clean) and `npx vitest run` (expect the new store tests + existing suite green).

- [ ] **Step 9: Commit**

```bash
git add src/lib/community-initial-sync.ts src/lib/community-initial-sync.test.ts src/App.svelte src/lib/components/ChannelMembersPanel.svelte src/lib/components/CommunityView.svelte
git commit -m "feat(invite): ZEB-949 graceful initial-sync UX for slim-bootstrap joins"
```

---

### Task 5: Full gate + PR

**Files:** none (validation + PR).

- [ ] **Step 1: Full Rust gate (CI parity)**

Run: `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
Then: `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` and `cargo fmt --all -- --check`.
Expected: all green.

- [ ] **Step 2: Full frontend gate**

Run (repo root): `npx tsc --noEmit` and `npx vitest run`.
Expected: all green.

- [ ] **Step 3: Confirm working tree clean, then open the PR**

```bash
git status --short   # expect empty
git push -u origin zeblith/zeb-949-slim-bootstrap-community-invites-strip-roster
gh pr create --repo zeblithic/harmony-client --base main \
  --title "feat(invite): slim-bootstrap community invites — strip roster (ZEB-949)" \
  --body "$(cat <<'EOF'
Phase 2 of ZEB-947. Stops inlining the O(members) roster in community invites;
members/channels/policies sync P2P after redemption. Invite size becomes O(1) —
a 500-member community's invite fits a plain Discord message (size fixture proves
it). Safe per the ZEB-947 membership-gate spike: the roster is a UI hint, never
consulted by the membership-at-HLC gate.

- Encoder emits an empty `state_snapshot` (one block in `generate_invite_impl`);
  no wire-format change; existing invites still redeem.
- Frontend shows "Syncing members… / Syncing channels…" during the ~1s sync
  window instead of misleading empty states.
- Regression: roster-less bootstrap gate tests + codec size fixture.

Design spec: docs/superpowers/specs/2026-08-16-zeb947-phase2-slim-bootstrap-design.md
Closes ZEB-949.
EOF
)"
```

Then follow the established review flow (fire CodeRabbit once at open; converge findings in one bundle; do not merge — operator-only).

---

## Notes for the executor

- **Optional stretch (not gated):** an engine-layer e2e in `SimCommunity` (`src-tauri/src/simnet/community.rs`) where an asymmetrically-seeded joiner (admin-only, not cross-seeded) converges to the full roster via bus sync. Skip unless time allows; Tasks 1–2 cover the property.
- **`initialSyncing` clear-on-content vs first-event:** clear on first *content* (a member beyond self, or any channel), never on the first empty sync event, to avoid an empty→populate flicker. The ~10s timeout is the safety-valve for a join-then-offline community.
- **Do not** touch `pre_fork_snapshot`, add a wire-format version byte, or change the `state_snapshot` field type — all explicitly out of scope (see Global Constraints).
