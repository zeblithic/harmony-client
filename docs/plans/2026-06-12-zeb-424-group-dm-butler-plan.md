# ZEB-424 — SP2 P3b Group-DM Butler Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the butler accept a deposit from a non-friend group-DM co-member by extending the deposit acceptor's admission with a local-knowledge co-membership check, so group-DM messages to an offline co-member get stored-and-forwarded like 1:1 DMs.

**Architecture:** Admission (`handle_deposit_core` step 1) becomes "Active friend **OR** live-GroupDm co-member". Co-membership is read from the butler's own replicated `OwnerState.spaces` via a new `ButlerDepositCtx::shares_live_group_dm` method backed by a pure free function `shares_live_group_dm_in`. The cert master-pin (step 2) keeps the friend path byte-for-byte unchanged (`cert_master == friend_master`) and adds a co-member branch that derives the anchor from the owner id (`owner_id_from_master_ed25519(cert_master) == sender_owner`). No wire-format change — `space_id` already travels inside the sealed payload and is validated by the unchanged inner verify. The `NotFriend` reject is renamed/absorbed into `NotAuthorized`.

**Tech Stack:** Rust, `async_trait`, tokio Mutex, `cargo nextest`, the existing `iroh_butler_acceptor` ctx-injection test pattern.

**Spec:** `docs/specs/2026-06-12-zeb-424-group-dm-butler-design.md` (D27–D34, esp. **D29.1** cert anchor). **Branch:** `zeb-424-group-dm-butler` (off main `fa6672c5`, spec already committed).

**House rules (every task):** no worktrees; `set -o pipefail`; commit BEFORE running gates; 10-minute wall-clock kill switch on any cargo command (use the Bash tool `timeout` param — macOS has no `timeout` binary); `--locked` always. Per-task gates unless a task says otherwise:
```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked -p harmony-app --lib --features test-fixtures
```
Reserve `--all-targets` for the final sweep (Task 6) — it relinks ~97 integration binaries (~25min compile / ~27min clippy).

---

### Task 1: Pure co-membership predicate `shares_live_group_dm_in` (TDD)

A pure function over `OwnerState` so the scan is unit-testable without building a `ProdButlerDepositCtx`. Lives in `iroh_butler_acceptor.rs` next to the acceptor.

**Files:**
- Modify: `src-tauri/src/iroh_butler_acceptor.rs` (add free fn + its `#[cfg(test)]` tests)

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `src/iroh_butler_acceptor.rs` (near the top of the test module, after the existing imports/helpers):

```rust
#[test]
fn shares_live_group_dm_in_matches_only_live_group_with_both_members() {
    use crate::owner_state_crdt::OwnerState;
    use crate::owner_state_types::{
        Hlc, OwnerAddr, Space, SpaceId, SpaceKind,
    };

    let me = [0x11u8; 16];
    let peer = [0x22u8; 16];
    let stranger = [0x33u8; 16];

    let hlc = Hlc { wall_ms: 1, logical: 0, device_id: "t".into() };
    let mk_space = |id: u8, kind: SpaceKind, members: Vec<[u8; 16]>, left: Option<Hlc>| Space {
        id: SpaceId([id; 16]),
        kind,
        parent: None,
        community_id: None,
        name: "g".into(),
        transport: None,
        members: members.into_iter().map(OwnerAddr).collect(),
        custom_name: None,
        notification_pref: None,
        left_at: left,
        created_at: hlc.clone(),
        updated_at: hlc.clone(),
        content_key: None,
        prior_content_keys: vec![],
        current_epoch: None,
        current_epoch_key: None,
        old_epoch_keys: std::collections::BTreeMap::new(),
        admin_addr: None,
        is_invite_only: None,
        shared_in_profile: false,
        pending_join_at: None,
    };

    let mut state = OwnerState::default();
    // A live GroupDm with both me and peer → match.
    let s_live = mk_space(0x01, SpaceKind::GroupDm, vec![me, peer], None);
    state.spaces.insert(s_live.id, s_live);
    assert!(shares_live_group_dm_in(&state, &me, &peer), "live group, both members");

    // Stranger is not a member → no match.
    assert!(!shares_live_group_dm_in(&state, &me, &stranger), "stranger not a member");

    // A LEFT GroupDm (left_at = Some) → no match.
    let mut state_left = OwnerState::default();
    let s_left = mk_space(0x02, SpaceKind::GroupDm, vec![me, peer], Some(hlc.clone()));
    state_left.spaces.insert(s_left.id, s_left);
    assert!(!shares_live_group_dm_in(&state_left, &me, &peer), "left group does not match");

    // A non-GroupDm space (Dm) with both members → no match (kind gate).
    let mut state_dm = OwnerState::default();
    let s_dm = mk_space(0x03, SpaceKind::Dm, vec![me, peer], None);
    state_dm.spaces.insert(s_dm.id, s_dm);
    assert!(!shares_live_group_dm_in(&state_dm, &me, &peer), "Dm kind does not match");

    // A GroupDm where SELF is absent (only peer) → no match.
    let mut state_noself = OwnerState::default();
    let s_noself = mk_space(0x04, SpaceKind::GroupDm, vec![peer, stranger], None);
    state_noself.spaces.insert(s_noself.id, s_noself);
    assert!(!shares_live_group_dm_in(&state_noself, &me, &peer), "self must be a member too");
}
```

- [ ] **Step 2: Run it to verify it fails to compile (function not defined)**

```bash
cd src-tauri && set -o pipefail
cargo nextest run --locked -p harmony-app --lib --features test-fixtures \
  -E 'test(shares_live_group_dm_in_matches_only_live_group_with_both_members)' 2>&1 | tail -20
```
Expected: compile error `cannot find function shares_live_group_dm_in`.

- [ ] **Step 3: Implement the pure function**

Add near the top of `src/iroh_butler_acceptor.rs` (module scope, after the `use` block, before `handle_deposit_core`):

```rust
/// ZEB-424 (D27): does the butler share a LIVE group-DM space with
/// `sender_owner`? Pure scan over the replicated `OwnerState.spaces` — the
/// same state step-1 admission already reads the friend graph from. A match
/// requires a `GroupDm` space that has not been left, with BOTH this owner
/// and the sender in `members`. Spaces count is small (tens), so a linear
/// scan needs no index (a derived index would add CRDT-merge invalidation
/// hazards for zero measured win).
pub(crate) fn shares_live_group_dm_in(
    state: &crate::owner_state_crdt::OwnerState,
    self_owner: &[u8; 16],
    sender_owner: &[u8; 16],
) -> bool {
    use crate::owner_state_types::{OwnerAddr, SpaceKind};
    let self_addr = OwnerAddr(*self_owner);
    let sender_addr = OwnerAddr(*sender_owner);
    state.spaces.values().any(|s| {
        s.kind == SpaceKind::GroupDm
            && s.left_at.is_none()
            && s.members.contains(&self_addr)
            && s.members.contains(&sender_addr)
    })
}
```

- [ ] **Step 4: Run the test to verify it passes**

```bash
cd src-tauri && set -o pipefail
cargo nextest run --locked -p harmony-app --lib --features test-fixtures \
  -E 'test(shares_live_group_dm_in_matches_only_live_group_with_both_members)' 2>&1 | tail -5
```
Expected: `1 passed`.

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/iroh_butler_acceptor.rs
git commit -m "feat(zeb-424): pure shares_live_group_dm_in predicate over OwnerState.spaces"
```

---

### Task 2: Add `shares_live_group_dm` to the ctx trait + all impls (compile unit)

Adding a trait method breaks every `ButlerDepositCtx` impl until each implements it, so the trait method + production impl + both mock impls land together. Admission is NOT rewired yet (that is Task 3), so existing behavior is unchanged and all current tests stay green.

**Files:**
- Modify: `src-tauri/src/iroh_butler_acceptor.rs` (trait + `ProdButlerDepositCtx` impl + `TestCtx` impl)
- Modify: `src-tauri/tests/butler_deposit_integration.rs` (its mock ctx impl)

- [ ] **Step 1: Add the trait method**

In the `pub trait ButlerDepositCtx` definition (after `lookup_friend`), add:

```rust
    /// ZEB-424 (D27): admission fallback when `lookup_friend` is not
    /// Active — `true` iff `sender_owner` shares a live `GroupDm` space
    /// with this owner. Production reads `OwnerState.spaces` under the
    /// CRDT lock via [`shares_live_group_dm_in`].
    async fn shares_live_group_dm(&self, sender_owner: &[u8; 16]) -> bool;
```

- [ ] **Step 2: Implement in `ProdButlerDepositCtx`**

In `impl ButlerDepositCtx for ProdButlerDepositCtx`, after `lookup_friend`, add:

```rust
    async fn shares_live_group_dm(&self, sender_owner: &[u8; 16]) -> bool {
        let state = self.crdt_state.lock().await;
        shares_live_group_dm_in(&state, &self.self_owner, sender_owner)
    }
```

- [ ] **Step 3: Implement in the `TestCtx` mock**

Add a field to `struct TestCtx` (after `friends`):

```rust
        /// ZEB-424: owners that share a live group-DM with self (the
        /// `shares_live_group_dm` source). Empty by default.
        group_co_members: std::collections::BTreeSet<[u8; 16]>,
```

Initialise it in `TestCtx::for_fixture` (add to the `Self { ... }` literal):

```rust
                group_co_members: std::collections::BTreeSet::new(),
```

Add the impl method to `impl ButlerDepositCtx for TestCtx` (after `lookup_friend`, so the order-probe records the call):

```rust
        async fn shares_live_group_dm(&self, sender_owner: &[u8; 16]) -> bool {
            self.push_event("group_lookup");
            self.group_co_members.contains(sender_owner)
        }
```

- [ ] **Step 4: Implement in the integration mock ctx**

In `src-tauri/tests/butler_deposit_integration.rs`, find the mock `impl ButlerDepositCtx` (the struct documented as mirroring `ProdButlerDepositCtx` "method-for-method", ~line 265). Add a `group_co_members: std::collections::BTreeSet<[u8; 16]>` field to that struct, initialise it empty wherever the struct is constructed, and add:

```rust
    async fn shares_live_group_dm(&self, sender_owner: &[u8; 16]) -> bool {
        self.group_co_members.contains(sender_owner)
    }
```

(Match the exact field/import style already in that file — it uses `harmony_app::...` paths.)

- [ ] **Step 5: Run gates (compiles, existing tests unchanged)**

```bash
cd src-tauri && set -o pipefail
cargo fmt --all
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -3
cargo clippy --locked -p harmony-app --test butler_deposit_integration --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -3
cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(deposit)' 2>&1 | tail -8
```
Expected: clippy clean; existing acceptor tests still pass (admission not yet changed).

- [ ] **Step 6: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/iroh_butler_acceptor.rs src-tauri/tests/butler_deposit_integration.rs
git commit -m "feat(zeb-424): add shares_live_group_dm to ButlerDepositCtx + all impls"
```

---

### Task 3: Rewire admission — friend OR co-member, with the D29.1 cert anchor (TDD)

The behavioural core. Write the new unit tests first, then restructure steps 1–2 of `handle_deposit_core`.

**Files:**
- Modify: `src-tauri/src/iroh_butler_acceptor.rs` (`DepositReject`, `handle_deposit_core`, existing tests referencing `NotFriend`, new tests)

- [ ] **Step 1: Write the failing tests**

Add to `mod tests`. These reuse the existing `Fixture` (its sender already satisfies `owner_id_from_master_ed25519(sender_master) == sender_owner`, being a real minted identity) and the `TestCtx` ctx-injection style:

```rust
#[tokio::test]
async fn deposit_from_non_friend_group_co_member_is_accepted() {
    let f = Fixture::build();
    // Not a friend, but a live group-DM co-member.
    let mut ctx = TestCtx::for_fixture(&f);
    ctx.friends.clear();
    ctx.group_co_members.insert(f.sender_owner);
    let ack = handle_deposit_core(&f.frame, &ctx).await.expect("co-member admitted");
    assert_eq!(ack.space_id, f.expected_space_id());
    // Admission consulted the group check only AFTER the friend miss.
    let ev = ctx.events();
    let fp = ev.iter().position(|e| e == "friend_lookup").unwrap();
    let gp = ev.iter().position(|e| e == "group_lookup").unwrap();
    assert!(fp < gp, "friend lookup precedes group lookup: {ev:?}");
}

#[tokio::test]
async fn deposit_from_active_friend_still_accepted_without_group() {
    let f = Fixture::build();
    let ctx = TestCtx::for_fixture(&f); // friend-Active, no group
    handle_deposit_core(&f.frame, &ctx).await.expect("friend still admitted");
}

#[tokio::test]
async fn deposit_from_neither_friend_nor_co_member_rejected_before_decrypt() {
    let f = Fixture::build();
    let ctx = TestCtx { friends: BTreeMap::new(), ..TestCtx::for_fixture(&f) };
    // group_co_members defaults empty → neither.
    let err = handle_deposit_core(&f.frame, &ctx).await.expect_err("rejected");
    assert!(matches!(err, DepositReject::NotAuthorized));
    // Reject is pre-decrypt: the decrypt probe never fired.
    assert!(!ctx.events().iter().any(|e| e == "decrypt"), "no decrypt on reject: {:?}", ctx.events());
}

#[tokio::test]
async fn co_member_with_forged_cert_master_rejected_bad_cert() {
    // Admission passes (co-member) but the cert's master does NOT hash to
    // sender_owner → the derived D29.1 anchor rejects with BadCert.
    let f = Fixture::build_with_foreign_cert_master();
    let mut ctx = TestCtx::for_fixture(&f);
    ctx.friends.clear();
    ctx.group_co_members.insert(f.sender_owner);
    let err = handle_deposit_core(&f.frame, &ctx).await.expect_err("forged master rejected");
    assert!(matches!(err, DepositReject::BadCert));
}

#[tokio::test]
async fn friend_path_still_pins_master_mismatch_rejected() {
    // Friend-Active but the friend-graph pinned master != cert master →
    // BadCert via the UNCHANGED friend branch (regression guard).
    let f = Fixture::build();
    let mut friends = BTreeMap::new();
    friends.insert(f.sender_owner, ([0xAAu8; 32], FriendStatus::Active)); // wrong pinned master
    let ctx = TestCtx { friends, ..TestCtx::for_fixture(&f) };
    let err = handle_deposit_core(&f.frame, &ctx).await.expect_err("pinned mismatch rejected");
    assert!(matches!(err, DepositReject::BadCert));
}
```

> **Note for the implementer:** `Fixture` may not yet expose `expected_space_id()` or `build_with_foreign_cert_master()`. Read the existing `Fixture` impl. If `expected_space_id()` is absent, assert on whatever the existing accepted-deposit test asserts about the ack instead (mirror `deposit_from_active_friend_is_accepted_persisted_then_acked`). For `build_with_foreign_cert_master()`: add a `Fixture` constructor that builds the deposit frame with an `EnrollmentCert` issued by a DIFFERENT master key (so `owner_id_from_master_ed25519(cert_master) != sender_owner`) while keeping `cert.owner_id == sender_owner` — reuse the existing cert-minting helper with a fresh master seed. If that is too invasive, drop this single test and rely on the integration test plus the friend-path regression test; note the omission in the commit body.

- [ ] **Step 2: Run to verify failure**

```bash
cd src-tauri && set -o pipefail
cargo nextest run --locked -p harmony-app --lib --features test-fixtures \
  -E 'test(group_co_member) | test(neither_friend_nor_co_member) | test(forged_cert_master) | test(friend_path_still_pins)' 2>&1 | tail -20
```
Expected: compile error or assertion failures (`NotAuthorized` variant missing; admission still rejects co-members at `NotFriend`).

- [ ] **Step 3: Rename `NotFriend` → `NotAuthorized` in `DepositReject`**

Replace the `NotFriend` variant:

```rust
    /// `frame.sender_owner` is neither an `Active` friend nor a live
    /// group-DM co-member (ZEB-424). All non-authorized senders collapse
    /// here — the wire close is uniform, so this distinction is only for
    /// counters/tests. (Formerly `NotFriend`.)
    #[error("sender is not authorized to deposit (not an active friend or co-member)")]
    NotAuthorized,
```

Update every reference: `grep -rn 'DepositReject::NotFriend' src/` and replace each with `NotAuthorized` (call sites + any tests; the stream-close/counter mapping stays identical).

- [ ] **Step 4: Restructure steps 1–2 of `handle_deposit_core`**

Replace the current step-1 friend lookup AND the step-2 `cert_master == friend_master` check with the two-variant admission verdict. The new step 1:

```rust
    // Step 1 — admission (spec §4 D5 as amended by ZEB-424 D27/D29.1): the
    // sender must be either an Active friend (pinned-master trust) OR a live
    // group-DM co-member (owner-id-derived trust). Friend status is checked
    // first; a non-Active result (Pending/Revoked/None) falls through to the
    // co-membership check — group membership is independent of friend status.
    enum Admission {
        /// Active friend: step 2 pins the cert master against this stored key.
        Friend([u8; 32]),
        /// Live group-DM co-member: step 2 derives the anchor from the owner id.
        CoMember,
    }
    let admission = match ctx.lookup_friend(&frame.sender_owner).await {
        Some((friend_master, FriendStatus::Active)) => Admission::Friend(friend_master),
        _ => {
            if ctx.shares_live_group_dm(&frame.sender_owner).await {
                Admission::CoMember
            } else {
                return Err(DepositReject::NotAuthorized);
            }
        }
    };
```

Then, in step 2, after computing `cert_master` and checking `cert.owner_id == frame.sender_owner`, replace the `cert_master != friend_master` check with the per-variant binding:

```rust
    if cert.owner_id != frame.sender_owner {
        return Err(DepositReject::BadCert);
    }
    // Master binding (D29.1): the friend path keeps its byte-for-byte pin
    // against the stored master; the co-member path derives the anchor from
    // the owner id (the owner id IS the hash of the master bundle —
    // `owner_id_from_master_ed25519`, the invariant
    // `iroh_friend_acceptor::master_ed25519_from_cert_matches_owner_id` pins).
    match admission {
        Admission::Friend(friend_master) => {
            if cert_master != friend_master {
                return Err(DepositReject::BadCert);
            }
        }
        Admission::CoMember => {
            if crate::friend_graph::owner_id_from_master_ed25519(&cert_master)
                != crate::owner_state_types::OwnerAddr(frame.sender_owner)
            {
                return Err(DepositReject::BadCert);
            }
        }
    }
```

> Keep the existing `let device_vk_bytes = cert.device_pubkeys.classical.ed25519_verify;` line and everything from step 3 onward UNCHANGED. Confirm `cert_master` is `[u8; 32]` and `owner_id_from_master_ed25519` returns `OwnerAddr`; wrap `frame.sender_owner` in `OwnerAddr(..)` for the comparison (as shown).

- [ ] **Step 5: Run tests to verify pass**

```bash
cd src-tauri && set -o pipefail
cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(deposit) | test(group_co_member) | test(shares_live_group_dm)' 2>&1 | tail -12
```
Expected: all green, including the new co-member/forged-master/friend-pin tests and the unchanged friend/cap/dup tests.

- [ ] **Step 6: Full gates + commit**

```bash
cd src-tauri && set -o pipefail
cargo fmt --all && cargo fmt --all -- --check
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -3
cargo nextest run --locked -p harmony-app --lib --features test-fixtures 2>&1 | tail -3
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/iroh_butler_acceptor.rs
git commit -m "feat(zeb-424): admit group-DM co-members at deposit (friend OR co-member; D29.1 cert anchor)"
```

---

### Task 4: Characterization tests for per-recipient mixed-state candidacy (D31)

D31 says "confirm, don't change": prove the existing outbox candidacy already does the right thing when one fan-out entry has recipients in mixed states. These are characterization tests on existing behavior — if one reveals a real deviation, STOP and report it (do not silently change production).

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` (add tests to its `#[cfg(test)] mod`)

- [ ] **Step 1: Read the existing outbox tests + drain logic**

Read `src/dm_outbox.rs` around `drain_phase_c` (~lines 1040–1290), `push_deposit_candidate` (~1262), `AttemptState`, `DEPOSIT_NOACK_WINDOWS`, `DeliveryStatus`, and the existing `mod tests` builders (e.g. `OwnerState::default()` usage at ~2869). Identify the existing helper that constructs an `OutboxEntry` with multiple `recipient_owners` and drives a drain pass.

- [ ] **Step 2: Write the characterization test**

Mirror the nearest existing drain test. Construct one `OutboxEntry` with three `recipient_owners` (A, B, C); set `AttemptState` so A is acked (terminal), B has `failure_count >= DEPOSIT_NOACK_WINDOWS` (deposit-candidate), C is fresh (pending, below threshold). Run the candidacy pass and assert:

```rust
// Only B becomes a deposit candidate.
assert_eq!(candidates.iter().map(|r| r.recipient_owner).collect::<Vec<_>>(), vec![B]);
// The entry stays Partial (a deposit is a relay, not a direct ack).
assert_eq!(entry_status, DeliveryStatus::Partial);
// A and C AttemptState are untouched by B's candidacy.
```

Use the actual field/method names found in Step 1 (the snippet above is the shape, not verbatim API). If the existing API does not expose candidates as a returnable list, assert via the side effect the existing tests use (e.g. the pushed `ButlerDepositRequest`s on a test channel).

- [ ] **Step 3: Run**

```bash
cd src-tauri && set -o pipefail
cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(mixed_state) | test(fan_out) | test(deposit_candidate)' 2>&1 | tail -8
```
Expected: PASS (confirming existing behavior). If RED for a real reason, STOP and report — that is a finding, not a test to force green.

- [ ] **Step 4: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/dm_outbox.rs
git commit -m "test(zeb-424): characterize per-recipient mixed-state deposit candidacy (D31)"
```

---

### Task 5: Integration — group-DM co-member end-to-end + non-member rejected

**Files:**
- Modify: `src-tauri/tests/butler_deposit_integration.rs`

- [ ] **Step 1: Read the existing end-to-end test**

Read the existing accepted-deposit integration test (the friend happy-path) and the mock ctx (now carrying `group_co_members` from Task 2). Note how it builds the deposit frame, runs `handle_deposit_core`, and asserts persist+ack + ingest delivery.

- [ ] **Step 2: Add the two tests**

```rust
// Non-friend co-member: butler admits, persists, acks; ingest delivers.
// (Clone the existing friend happy-path test, then before running the
// deposit: clear the friend edge and insert sender into group_co_members.)
```
and
```rust
// Non-member, non-friend: handle_deposit_core returns Err(NotAuthorized);
// assert nothing was persisted into the dm-inbox store.
```
Use the existing test's exact construction; the only deltas are the ctx's `friends`/`group_co_members` maps and the expected result.

- [ ] **Step 3: Run (integration gate)**

```bash
cd src-tauri && set -o pipefail
cargo clippy --locked -p harmony-app --test butler_deposit_integration --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -3
cargo nextest run --locked -p harmony-app --test butler_deposit_integration --features test-fixtures 2>&1 | tail -8
```
Expected: all green incl. the two new tests.

- [ ] **Step 4: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/tests/butler_deposit_integration.rs
git commit -m "test(zeb-424): group-DM co-member deposit end-to-end + non-member rejected"
```

---

### Task 6: Wire-pin assertion + final `--all-targets` sweep

**Files:**
- (Verify only) `src-tauri/src/butler_deposit.rs` — `deposit_frame_wire_fixture_pinned`

- [ ] **Step 1: Confirm the wire fixture is unchanged**

No deposit-frame field changed (D28), so the pin must still pass with zero edits — this is itself the assert that the no-wire-change property holds.

```bash
cd src-tauri && set -o pipefail
cargo nextest run --locked -p harmony-app --lib --features test-fixtures \
  -E 'test(deposit_frame_wire_fixture_pinned)' 2>&1 | tail -5
```
Expected: PASS, no code change. If it FAILS, a wire field was changed accidentally — STOP and revert that change.

- [ ] **Step 2: Final full sweep (the load-bearing one — relinks integration binaries)**

Run with a generous wall-clock budget (use the Bash tool `timeout` param, e.g. 1800000 ms; supervise per the long-running-supervision rule):

```bash
cd src-tauri && set -o pipefail
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
cargo nextest run --locked --all-targets --features test-fixtures 2>&1 | tail -8
```
Expected: clippy clean; nextest green except the known pre-existing env flakes (iroh/zenoh transport orphans, `rename_content_integration` per ZEB-420 — record, don't chase). The new acceptor/outbox/integration tests all pass.

- [ ] **Step 3: Commit (if the sweep required any fmt/clippy fixups)**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add -A src-tauri
git commit -m "chore(zeb-424): final --all-targets sweep fixups" || echo "nothing to fix up"
```

---

## After all tasks

1. Final code review pass over the diff (admission ordering, the D29.1 cert anchor for both branches, no wire change, `NotFriend`→`NotAuthorized` fully swept).
2. Push `zeb-424-group-dm-butler`; open the PR. **PR body references ONLY ZEB-424** (Linear closes every ZEB-NNN in a merged body — no other ticket ids, no ticket-numbered file paths). Reference the spec + this plan by path. Never write the at-mention form of greptile.
3. Comment on the ZEB-424 Linear thread at PR-open.
4. Run the autonomous bot + CI convergence loop (scan all three comment buckets; ScheduleWakeup self-pacing; ONE push per round with a visible hold-push signal). Pushover Jake at ready-to-merge. Do NOT self-merge.

## Self-review notes (author)

- **Spec coverage:** D27 (Task 3 admission), D28 (Task 6 wire-pin), D29 (Task 2 trait method + Task 3 `NotAuthorized`), **D29.1** (Task 3 cert-anchor branch + tests), D30 (no code — eventual-consistency is inherent), D31 (Task 4), D32/D33 (no code — bounded/unchanged), D34 (Tasks 1,3,4,5 tests). All covered.
- **Type consistency:** `shares_live_group_dm_in(&OwnerState, &[u8;16], &[u8;16]) -> bool` (Task 1) is called by `ProdButlerDepositCtx::shares_live_group_dm` (Task 2) and the trait method `shares_live_group_dm(&self, &[u8;16]) -> bool` (Task 2) is consumed in admission (Task 3). `owner_id_from_master_ed25519(&[u8;32]) -> OwnerAddr` compared against `OwnerAddr(frame.sender_owner)`. `DepositReject::NotAuthorized` defined Task 3 Step 3, used Task 3 Step 4 + tests.
- **Known soft spots flagged inline:** `Fixture::expected_space_id()` / `build_with_foreign_cert_master()` may need adding — the implementer is told to read `Fixture` and adapt or drop-with-note. Task 4's outbox API names are shape-not-verbatim — the implementer is told to mirror the nearest existing drain test.
