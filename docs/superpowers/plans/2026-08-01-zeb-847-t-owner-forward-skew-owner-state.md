# ZEB-847 T-OWNER: Bounded Forward-Skew at Owner-State CRDT Merge — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bound every peer/sibling-supplied wall-clock stamp at the owner-state CRDT merge boundary so a skewed or compromised own-device can no longer pin a revocation/privacy control off (or on) forever.

**Architecture:** The owner-state remote-merge is a single pure free function, `merge_remote_into_local(local, remote)` (`owner_state_sync.rs:251`), that folds every owner sub-CRDT. Sample the receiver's own wall clock **once** at the top (`clock_trust::receiver_now_ms()`), then apply one of two guards per field: **reject/skip** a remote entry whose deciding HLC wall is implausibly future (LWW-replace-by-HLC registers — Friend, Space, owner-device, read-marker, Library), or **clamp** an incoming raw-`u64` stamp down to `now + MAX_FORWARD_SKEW_MS` before a grow-only `max` join (Grant `granted_at`/`revoked_at`, received-grant dismiss/`received_at`). Notes merge through a **separate** engine (`NotesDoc::merge_from`, `notes_crdt.rs:91`) and get the same reject at their own sample point. `receiver_now_ms()` returning `None` (unreadable/pre-epoch clock) means **apply-all** everywhere — a bad *local* clock must never drop honest owner state.

**Tech Stack:** Rust (workspace crate `harmony-app` under `src-tauri/`), `cargo nextest`, the ZEB-831 `clock_trust` policy module (already in-tree from ZEB-846).

## Global Constraints

Every task's requirements implicitly include this section. Values are verbatim.

- **Policy module is the one auditable home.** All forward-skew logic uses `crate::clock_trust`: the constant `MAX_FORWARD_SKEW_MS` (`= 5*60*1000`), `reject_future(stamp, now, tol)` (returns `true` when `stamp.saturating_sub(now) > tol` — boundary **inclusive**, `stamp == now+tol` is accepted), `clamp_future(stamp, now, tol)` (returns `stamp.min(now+tol)`), and `receiver_now_ms() -> Option<u64>`. Do **not** hand-roll `SystemTime::now()` at a merge site or introduce a second constant.
- **Receiver-`now` is `receiver_now_ms()` ONLY.** Never a peer-supplied, HLC, or adoption-nudged value — those are exactly the clocks the bound distrusts.
- **`None` ⇒ apply-all, NEVER `0`.** When `receiver_now_ms()` is `None`, every guard is a no-op (merge everything). Substituting `0` would make every honest present-day wall (~1.7e12 ms) exceed the ceiling and reject *all* owner state — the inversion of the invariant that a bad LOCAL clock must never drop honest state.
- **Forward bounds only.** Every T-OWNER guard rejects/clamps *future* stamps. Do **not** add a backward/anti-backdating guard at any owner-state merge site (the sole backward-bound site in the threat model, relay-hold, is out of scope). `created_at` backdating on Space is already defended structurally at `owner_state_crdt.rs:361-382`; do not touch it.
- **Reject in place — no destructive write-back.** A rejected/skipped remote entry means "keep local, ignore the poisoned incoming this round." Never persist a *mutated/erased* copy back, and never delete local state on a slow clock: the merge must be self-healing (the peer re-offers its snapshot next sync round; once the clock is within tolerance the entry merges). This is the ZEB-621/831 "gate the decision, not a destructive store rewrite" rule.
- **Sample `receiver_now` ONCE per merge**, at the top of the merge function (mirrors `community_state_sync.rs:4353`). Do not re-sample per entry.
- **Sign of the guard per site is load-bearing:** LWW-replace-by-HLC ⇒ **reject/skip** (clamping is insufficient — a clamped `now+5min` still out-stamps an honest `now`). Grow-only `max`-join of raw `u64` ⇒ **clamp** (preserves the CRDT join; the local revoke, which stamps `now.max(granted_at)`, can always catch a clamped grant).
- **One positive-discrimination test per site:** a poisoned stamp set *higher* than a legitimate opposing one, asserting the legitimate control still wins after merge — so any leak clamps visibly (the ZEB-790 T5–T7 / ZEB-846 pattern). Tests run at real wall time: build the honest stamp from `SystemTime::now()` (ms) and the poisoned stamp `now + 400 days`; the 5-min tolerance dwarfs test execution time, so this is deterministic without clock mocking.
- **CI gates (run from `src-tauri/`):** `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. MSRV is 1.91 (`Option::is_some_and`/`map_or`/`is_none_or` all available). Iterative gating may use `scripts/test-select --context task`; the **final** pre-PR gate is the full `--workspace --all-targets` sweep. Paste the `round=… bucket=…` line from `test-select` into task reports.

---

## File Structure

No new files. Three modifications:

- **`src-tauri/src/clock_trust.rs`** — add two `Option`-gated policy helpers that centralize the `None ⇒ apply-all` + control-tier-ceiling contract, so every merge site is uniform and auditable in one place. Plus their unit tests.
- **`src-tauri/src/owner_state_sync.rs`** — sample `receiver_now` once in `merge_remote_into_local`; add the reject guards (Friend, Space, owner-device, Library, read-marker) and the clamps (Grant, dismiss, received-grant). Add discrimination tests in the file's `#[cfg(test)] mod debounce_tests` (or a new sibling test module).
- **`src-tauri/src/notes_crdt.rs`** — sample `receiver_now` in `NotesDoc::merge_from`; reject a future `updated_at`. Add a test.

---

## Task 1: Policy helpers in `clock_trust`

**Files:**
- Modify: `src-tauri/src/clock_trust.rs` (add two `pub fn` after `receiver_now_ms`, ~line 93; tests in the existing `mod tests`)

**Interfaces:**
- Produces:
  - `pub fn wall_exceeds_forward_skew(wall_ms: u64, receiver_now_ms: Option<u64>) -> bool` — `true` iff `receiver_now_ms` is `Some(rn)` and `reject_future(wall_ms, rn, MAX_FORWARD_SKEW_MS)`; `None ⇒ false` (apply-all).
  - `pub fn clamp_wall_to_forward_skew(wall_ms: u64, receiver_now_ms: Option<u64>) -> u64` — `clamp_future(wall_ms, rn, MAX_FORWARD_SKEW_MS)` when `Some(rn)`, else `wall_ms` unchanged (apply-all).
- These are the only two entry points every owner-state merge site (Tasks 2–7) and the notes merge (Task 7... see note) will call.

- [ ] **Step 1: Write the failing tests** in `clock_trust.rs`'s `mod tests`:

```rust
#[test]
fn wall_exceeds_forward_skew_none_now_is_apply_all() {
    // Unreadable local clock ⇒ never reject (a bad LOCAL clock must not drop honest state).
    assert!(!wall_exceeds_forward_skew(u64::MAX, None));
    assert!(!wall_exceeds_forward_skew(0, None));
}

#[test]
fn wall_exceeds_forward_skew_honors_the_inclusive_ceiling() {
    let now = 1_700_000_000_000;
    assert!(!wall_exceeds_forward_skew(now, Some(now)), "present accepted");
    assert!(!wall_exceeds_forward_skew(now - 10_000, Some(now)), "past accepted");
    assert!(
        !wall_exceeds_forward_skew(now + MAX_FORWARD_SKEW_MS, Some(now)),
        "exactly at the ceiling is accepted (inclusive)"
    );
    assert!(
        wall_exceeds_forward_skew(now + MAX_FORWARD_SKEW_MS + 1, Some(now)),
        "one past the ceiling is rejected"
    );
}

#[test]
fn clamp_wall_to_forward_skew_caps_only_the_future() {
    let now = 1_700_000_000_000;
    assert_eq!(clamp_wall_to_forward_skew(now - 5, Some(now)), now - 5, "past unchanged");
    assert_eq!(
        clamp_wall_to_forward_skew(now + MAX_FORWARD_SKEW_MS + 10_000, Some(now)),
        now + MAX_FORWARD_SKEW_MS,
        "future capped to the ceiling"
    );
    assert_eq!(
        clamp_wall_to_forward_skew(u64::MAX, None),
        u64::MAX,
        "None ⇒ apply-all (unchanged)"
    );
}
```

- [ ] **Step 2: Run to verify they fail** — `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(wall_exceeds_forward_skew) + test(clamp_wall_to_forward_skew)'` — Expected: FAIL (functions not defined).

- [ ] **Step 3: Add the helpers** after `receiver_now_ms` in `clock_trust.rs`:

```rust
/// `true` iff `wall_ms` is implausibly far in the receiver's *future* under the
/// control-tier ceiling ([`MAX_FORWARD_SKEW_MS`]). `receiver_now_ms == None`
/// (unreadable / pre-epoch clock) ⇒ `false` (apply-all): a bad LOCAL clock must
/// never drop honest state. The forward-skew half of the T-OWNER (ZEB-847) and
/// T-GOV (ZEB-846) owner/governance merge bounds; boundary is inclusive.
#[inline]
pub fn wall_exceeds_forward_skew(wall_ms: u64, receiver_now_ms: Option<u64>) -> bool {
    receiver_now_ms.is_some_and(|rn| reject_future(wall_ms, rn, MAX_FORWARD_SKEW_MS))
}

/// Clamps `wall_ms` down to at most `receiver_now + MAX_FORWARD_SKEW_MS` for a
/// grow-only `max`-merged register, so a future-dated stamp cannot win the join
/// and pin the register forever. `receiver_now_ms == None` ⇒ unchanged
/// (apply-all). A past/present stamp is returned unchanged.
#[inline]
pub fn clamp_wall_to_forward_skew(wall_ms: u64, receiver_now_ms: Option<u64>) -> u64 {
    receiver_now_ms.map_or(wall_ms, |rn| clamp_future(wall_ms, rn, MAX_FORWARD_SKEW_MS))
}
```

- [ ] **Step 4: Run to verify they pass** — same command as Step 2 — Expected: PASS (3 tests).

- [ ] **Step 5: Commit** — `git add src-tauri/src/clock_trust.rs && git commit -m "ZEB-847: Option-gated forward-skew policy helpers for CRDT merge sites"`

---

## Task 2: Sample point + FriendEntry reject (CRITICAL — FR)

Finding FR (`owner_state_crdt.rs:1099`, FAIL-OPEN): a sibling writing `status:Active` at `wall+1yr` makes the friendship un-revokable — the DM cutoff is defeated (a blocked party keeps DM access). FriendEntry is LWW-by-`learned_at` (newer wins; `Revoked` is a tombstone), so a future `learned_at` blocks every later honest revoke. **Reject** the future entry so it never enters the register.

**Files:**
- Modify: `src-tauri/src/owner_state_sync.rs` (sample point at `merge_remote_into_local` top ~line 251; friend loop at 385-396)
- Test: same file, `#[cfg(test)] mod debounce_tests` (~line 578) — read the existing tests there first for the `OwnerState` / `FriendEntry` / `Hlc` builder idioms.

**Interfaces:**
- Consumes: `crate::clock_trust::{wall_exceeds_forward_skew, receiver_now_ms}` (Task 1).
- Produces: a `let receiver_now = crate::clock_trust::receiver_now_ms();` binding at the **top** of `merge_remote_into_local`, reused by Tasks 3–6. (Sampled once — do not re-sample.)
- Field: `FriendEntry.learned_at: Hlc`; the untrusted wall is `entry.learned_at.wall_ms: u64`. `FriendStatus` has `Active` / `Revoked` variants.

- [ ] **Step 1: Write the failing discrimination test** in `mod debounce_tests`. Shape (use the module's existing builders for `OwnerState`/`FriendEntry`/`Hlc`/`OwnerAddr`; `FriendStatus::Active`/`Revoked`):

```rust
#[test]
fn future_dated_active_friend_cannot_block_an_honest_revoke() {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let friend = /* some fixed OwnerAddr */;

    // Local state already holds an honest REVOKE at real `now`.
    let mut local = OwnerState::default();
    local.apply_friend_update(
        friend,
        friend_entry(friend, FriendStatus::Revoked, hlc_at(now_ms)),
    );

    // A malicious sibling snapshot re-activates the friendship, stamped 400 days
    // ahead so it would out-LWW the honest revoke forever.
    let mut remote = OwnerState::default();
    remote.apply_friend_update(
        friend,
        friend_entry(friend, FriendStatus::Active, hlc_at(now_ms + 400 * 24 * 60 * 60 * 1000)),
    );

    merge_remote_into_local(&mut local, remote);

    // The forward-skew reject drops the poisoned Active → the revoke stands.
    assert_eq!(
        local.friend_graph.friends.get(&friend).map(|e| &e.status),
        Some(&FriendStatus::Revoked),
        "future-dated Active must not resurrect a revoked friendship",
    );
}
```

Also add a companion asserting an **honest** re-activate at real `now` (within tolerance) *does* win, so the guard isn't over-rejecting:

```rust
#[test]
fn present_dated_active_friend_still_wins_normally() {
    let now_ms = /* as above */;
    let friend = /* same */;
    let mut local = OwnerState::default();
    local.apply_friend_update(friend, friend_entry(friend, FriendStatus::Revoked, hlc_at(now_ms)));
    let mut remote = OwnerState::default();
    // learned_at strictly newer than the revoke, but within the 5-min window.
    remote.apply_friend_update(friend, friend_entry(friend, FriendStatus::Active, hlc_at(now_ms + 1000)));
    merge_remote_into_local(&mut local, remote);
    assert_eq!(
        local.friend_graph.friends.get(&friend).map(|e| &e.status),
        Some(&FriendStatus::Active),
        "an in-window re-activate must merge normally",
    );
}
```

> If `hlc_at`/`friend_entry` helpers don't already exist in the module, write tiny local `fn`s that build a `FriendEntry` with the given `learned_at.wall_ms` and status (mirror how the existing tests construct these), and an `Hlc { wall_ms, logical: 0, device_id: "test".into() }`. Keep `logical`/`device_id` constant so `learned_at` ordering is decided purely by `wall_ms`.

- [ ] **Step 2: Run to verify the first test fails** — `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(future_dated_active_friend_cannot_block_an_honest_revoke)'` — Expected: FAIL (the poisoned Active currently wins; `status` is `Active`).

- [ ] **Step 3: Add the sample point + the friend reject.** At the top of `merge_remote_into_local`, immediately after the `let OwnerState { .. } = remote;` destructure (line 270), add:

```rust
    // ZEB-847 (T-OWNER): sample the receiver's OWN wall clock ONCE for the whole
    // merge. Every peer/sibling-supplied stamp below is bounded against this and
    // this only — never a peer/HLC/adoption value. `None` (unreadable clock) ⇒
    // apply-all: a bad LOCAL clock must never drop honest owner state.
    let receiver_now = crate::clock_trust::receiver_now_ms();
```

Then in the friend loop (385), skip a future-dated entry before applying:

```rust
    for (addr, entry) in friend_graph.friends {
        // ZEB-847: a future-dated `learned_at` would out-LWW every later honest
        // revoke forever (FAIL-OPEN: blocked party keeps DM access). Reject it —
        // clamping is insufficient (a clamped now+5min still beats an honest now).
        if crate::clock_trust::wall_exceeds_forward_skew(entry.learned_at.wall_ms, receiver_now) {
            continue;
        }
        if let crate::owner_state_crdt::ApplyOutcome::Rejected(
            reason @ crate::owner_state_crdt::RejectionReason::InvariantFail(_),
        ) = local.apply_friend_update(addr, entry)
        {
            tracing::warn!(
                addr = %hex::encode(addr.0),
                reason = %reason,
                "friend-graph merge rejected entry on invariant violation"
            );
        }
    }
```

- [ ] **Step 4: Run to verify both tests pass** — `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(future_dated_active_friend_cannot_block_an_honest_revoke) + test(present_dated_active_friend_still_wins_normally)'` — Expected: PASS.

- [ ] **Step 5: Commit** — `git add src-tauri/src/owner_state_sync.rs && git commit -m "ZEB-847: reject future-dated FriendEntry at owner-state merge (FR, CRITICAL)"`

---

## Task 3: Space `shared_in_profile` reject (CRITICAL — SP)

Finding SP (`owner_state_crdt.rs:1226`, FAIL-OPEN): a future-dated Space pins the community publicly listed and discards the user's later privacy opt-out. Space merges via `lww_merge_space` — `updated_at`-HLC LWW decides the winner (`shared_in_profile: newer.shared_in_profile`). A future `updated_at` is the pin vector. **Reject** the future Space at the merge boundary before `apply_space_with_canonicalization`.

`created_at` is **not** a vector here (creator-pin uses *earliest*, and backdating is already rejected by the immutable-field guard at `owner_state_crdt.rs:361-382`); bound `updated_at` only.

**Files:**
- Modify: `src-tauri/src/owner_state_sync.rs` (spaces loop at 305-307)
- Test: `mod debounce_tests`

**Interfaces:**
- Consumes: `receiver_now` (Task 2), `clock_trust::wall_exceeds_forward_skew`.
- Field: `Space.updated_at: Hlc` → `.wall_ms`; `Space.shared_in_profile: bool`.

- [ ] **Step 1: Write the failing test** — local Space with `shared_in_profile=false` at real `now` (an honest opt-out); remote same SpaceId with `shared_in_profile=true` at `now + 400 days`. After merge assert the local Space's `shared_in_profile` is still `false`. Add a companion where the remote opt-out change is at `now + 1000` (in-window) and *does* win. Use the module's Space builder; keep `SpaceId`, `admin_addr`, `created_at`, `is_invite_only` identical between local/remote so only `updated_at`/`shared_in_profile` differ (the immutable-field guard rejects mismatches).

- [ ] **Step 2: Run to verify it fails** — `-E 'test(future_dated_space...)'` — Expected: FAIL (`shared_in_profile` flips to `true`).

- [ ] **Step 3: Add the reject** in the spaces loop:

```rust
    for (_, space) in spaces {
        // ZEB-847: a future-dated `updated_at` would win the LWW and pin
        // `shared_in_profile` (privacy-control bypass), discarding a later
        // opt-out. Reject it; `created_at` is not a vector (earliest-wins +
        // immutable-field guard at owner_state_crdt.rs:361-382).
        if crate::clock_trust::wall_exceeds_forward_skew(space.updated_at.wall_ms, receiver_now) {
            continue;
        }
        local.apply_space_with_canonicalization(space);
    }
```

- [ ] **Step 4: Run to verify both tests pass.** — Expected: PASS.

- [ ] **Step 5: Commit** — `-m "ZEB-847: reject future-dated Space updated_at at merge (SP privacy bypass, CRITICAL)"`

---

## Task 4: owner-device, LibraryEntry, read-marker rejects (DEV / LE / RM)

Three more HLC-LWW-replace registers, same reject pattern as Task 2 (mechanical repeats against the established `receiver_now` + `wall_exceeds_forward_skew`). Each gets its own discrimination test.

- **DEV** (`owner_state_crdt.rs:886`): a future `learned_at` on the owner-device entry LWW-replaces and then blocks every later legit device update → the peer's new/rotated device is never learned, their DMs read `UnknownSigningKey`. Reject at the device loop (`owner_state_sync.rs:335`) before `apply_owner_device_update`. Field: `OwnerDeviceEntry.learned_at: Hlc`.
- **LE** (`owner_state_types.rs:2548`): the trusted-library set is LWW by `max(added_at, removed_at)`; a future `added_at` pins it present, a future `removed_at` pins it removed and blocks re-adds. Reject the remote entry at the library loop (`owner_state_sync.rs:352`) if **either** `added_at` **or** `removed_at` is future. Fields: `LibraryEntry.added_at: Hlc`, `removed_at: Option<Hlc>`.
- **RM** (`owner_state_crdt.rs:694`): a future `last_read_at` pins "everything read" and suppresses unread badges permanently. Reject at the marker loop (`owner_state_sync.rs:314`) before `apply_marker`. Field: `ReadMarker.last_read_at: Hlc`.

**Files:**
- Modify: `src-tauri/src/owner_state_sync.rs` (loops at 314, 335, 352)
- Test: `mod debounce_tests`

- [ ] **Step 1: Write three failing discrimination tests** (one per register), each: local holds the honest "control-applied" state at real `now` (device set with the *current* devices; library `removed`; marker unread / at an older `last_read_at`), remote carries the poisoned future entry, assert after merge that the honest state stands. For DEV specifically, assert the poison does **not** block a subsequent in-window legit device update — i.e. after the poisoned entry is rejected, a later `apply_owner_device_update` at real `now` still learns the new device. Add an in-window companion for at least the LE case (a legit `removed_at` at `now+1000` still removes).

- [ ] **Step 2: Run to verify they fail.**

- [ ] **Step 3: Add the three rejects:**

```rust
    // markers loop (314):
    for (_, marker) in markers {
        // ZEB-847: a future `last_read_at` would pin "all read" forever (RM).
        if crate::clock_trust::wall_exceeds_forward_skew(marker.last_read_at.wall_ms, receiver_now) {
            continue;
        }
        local.apply_marker(marker);
    }
```

```rust
    // owner_device_cache loop (335):
    for (addr, entry) in owner_device_cache.devices {
        // ZEB-847: a future `learned_at` would LWW-replace and then block every
        // later legit device update (DEV → peer devices unlearnable, DMs
        // UnknownSigningKey).
        if crate::clock_trust::wall_exceeds_forward_skew(entry.learned_at.wall_ms, receiver_now) {
            continue;
        }
        local.apply_owner_device_update(
            addr,
            entry.devices,
            entry.device_identity_pubs,
            entry.device_tunnel_contacts,
            entry.learned_at,
        );
    }
```

```rust
    // libraries loop (352):
    for (addr, remote_entry) in libraries {
        // ZEB-847: a future `added_at` pins the library present; a future
        // `removed_at` pins it removed and blocks re-adds (LE). Reject if either
        // bound is implausibly future.
        if crate::clock_trust::wall_exceeds_forward_skew(remote_entry.added_at.wall_ms, receiver_now)
            || remote_entry
                .removed_at
                .as_ref()
                .is_some_and(|rm| crate::clock_trust::wall_exceeds_forward_skew(rm.wall_ms, receiver_now))
        {
            continue;
        }
        let remote_max: &Hlc = match &remote_entry.removed_at {
            Some(rm) if rm.is_strictly_newer_than(&remote_entry.added_at) => rm,
            _ => &remote_entry.added_at,
        };
        let should_replace = match local.libraries.get(&addr) {
            None => true,
            Some(existing) => {
                let local_max: &Hlc = match &existing.removed_at {
                    Some(rm) if rm.is_strictly_newer_than(&existing.added_at) => rm,
                    _ => &existing.added_at,
                };
                remote_max.is_strictly_newer_than(local_max)
            }
        };
        if should_replace {
            local.libraries.insert(addr, remote_entry);
        }
    }
```

- [ ] **Step 4: Run to verify the three tests pass.**

- [ ] **Step 5: Commit** — `-m "ZEB-847: reject future-dated owner-device/library/read-marker entries at merge (DEV/LE/RM)"`

---

## Task 5: GrantEntry — reject future `granted_at`, clamp `revoked_at` (CRITICAL — GR)

Finding GR (`owner_state_types.rs:2578` / `owner_state_sync.rs:471`, FAIL-OPEN): file grants are an LWW-element-set — each grantee carries `granted_at` and `revoked_at`, both grow-only `max` joins; active iff `granted_at > revoked_at`. A skewed re-share (`granted_at = now+5m`, or far worse `now+1yr`) makes a later honest revoke lose the `>` and silently undoes the file-share revoke.

**Reject** a future-dated incoming `granted_at` — the FAIL-OPEN vector. A *clamp* is insufficient here (empirically confirmed, Task 5 round 0): `granted_at > revoked_at` is a **static** comparison never re-evaluated against advancing time, so a clamped `now+5min` still exceeds an honest revoke stamped at `now`, leaving the grant permanently active — the clamp only shrinks the poisoned magnitude (400d → 5min), not the qualitative fail-open. Rejecting the record (like FR/SP/LE, and matching the spec §6.2 *primary* `reject_future`-at-each-merge-boundary instruction) means the poisoned activation never lands, so a later honest re-grant (`granted_at` at real `now`, past `receiver_now`) is accepted normally while a future one is dropped. **Additionally clamp** the incoming `revoked_at` — the SAFE (deactivating) direction — so a future revoke can't over-apply and pin the register out of reach of a legit re-grant (grief-lockout). The spec's "clamp the revoke side against the merged granted stamp" nuance is already satisfied by `revoke_grant_inner`'s unchanged `revoked_at = now.max(granted_at)`.

**Files:**
- Modify: `src-tauri/src/owner_state_sync.rs` (file_grants loop at 462-486)
- Test: `mod debounce_tests`

**Interfaces:**
- Consumes: `receiver_now` (Task 2), `clock_trust::wall_exceeds_forward_skew` (for `granted_at`), `clock_trust::clamp_wall_to_forward_skew` (for `revoked_at`).
- Field: `GrantEntry { granted_at: u64, revoked_at: u64, .. }` (raw ms, not HLC).

- [ ] **Step 1: Write the failing discrimination test.** Local holds an honest **revoke** of a grant to grantee G at real `now` (build via `record_grant` then `revoke_grant_inner(&mut local, cid, g, now_ms)` — read `file_sharing.rs` for signatures). Remote carries the same grantee re-shared with `granted_at = now + 400 days` (and `revoked_at = 0`). After `merge_remote_into_local`, assert the grant is **inactive** — `granted_at <= revoked_at` for that grantee (the revoke stands). Without the reject, the poisoned `granted_at` wins the max and `granted_at > revoked_at` ⇒ active (leak). Add an in-window companion: a legit re-share at `now + 1000` after an *older* revoke does reactivate (guard isn't over-rejecting).

- [ ] **Step 2: Run to verify it fails** (grant reads active).

- [ ] **Step 3: Add the guard** at the top of the `for g in remote_grants` loop, before both the max-join and the `push`:

```rust
    for (cid, remote_grants) in file_grants {
        let entry = local.file_grants.entry(cid).or_default();
        for g in remote_grants {
            // ZEB-847 (GR, CRITICAL): `granted_at` is the FAIL-OPEN vector — a
            // future value wins the grow-only max and reactivates the grant past
            // an honest revoke. A clamp is INSUFFICIENT (a clamped now+5min still
            // exceeds an honest revoke at now, and `granted_at > revoked_at` is a
            // static compare never re-evaluated against time), so REJECT the
            // record outright — mirrors FR/SP/LE and the spec §6.2 primary
            // `reject_future`-at-merge-boundary instruction. A later honest
            // re-grant (granted_at at real now, past receiver_now) is accepted.
            if crate::clock_trust::wall_exceeds_forward_skew(g.granted_at, receiver_now) {
                continue;
            }
            // `revoked_at` is the SAFE (deactivating) direction, but a future
            // value over-revokes and pins the register out of reach of a legit
            // re-grant (grief-lockout). Clamp its magnitude before the max-join.
            let mut g = g;
            g.revoked_at = crate::clock_trust::clamp_wall_to_forward_skew(g.revoked_at, receiver_now);
            match entry.iter().position(|e| e.grantee_owner == g.grantee_owner) {
                Some(i) => {
                    if g.granted_at > entry[i].granted_at {
                        entry[i].granted_at = g.granted_at;
                    }
                    if g.revoked_at > entry[i].revoked_at {
                        entry[i].revoked_at = g.revoked_at;
                    }
                }
                None => entry.push(g),
            }
        }
        entry.sort_by(|a, b| {
            a.grantee_owner
                .cmp(&b.grantee_owner)
                .then(a.granted_at.cmp(&b.granted_at))
        });
    }
```

- [ ] **Step 4: Run to verify both tests pass.** (Both pass under reject: the poisoned `now+400d` re-share is dropped so the revoke stands; the in-window `now+1000` re-share is accepted and reactivates.)

- [ ] **Step 5: Commit** — `-m "ZEB-847: reject future-dated GrantEntry granted_at, clamp revoked_at at merge (GR revoke bypass, CRITICAL)"`

---

## Task 6: received-grant — reject future `received_at`, clamp `dismissed_at` (RG)

Finding RG (`owner_state_sync.rs:512-575`): received-file-grant activeness is `received_at > dismissed_at`, both `max`-merged `u64`. A future `received_at` makes the dismiss (and the ZEB-730 granter-revoke, which routes through the dismiss tombstone) a silent no-op; a future `dismissed_at` pins it dismissed.

Same structure as Task 5 (GR), same fix: `received_at` is the FAIL-OPEN vector (higher = grant active = un-dismissable), so a *clamp* is insufficient (a clamped `now+5min` `received_at` still exceeds an honest dismiss at `now`, and `received_at > dismissed_at` is a static compare). **Reject** a future incoming `received_at` — the poisoned re-supply never lands, so an existing dismiss sweeps the grant out and the dismiss stands. **Clamp** the incoming `dismissed_at` — the SAFE (deactivating) direction — to bound a future dismiss from grief-locking a legit re-share out of reach.

**Files:**
- Modify: `src-tauri/src/owner_state_sync.rs` (dismiss-tombstone loop 512-515; received-grant union loop 535-560)
- Test: `mod integration_tests` (co-locate with the other T-OWNER merge tests, ~line 1923+)

**Interfaces:**
- Consumes: `receiver_now` (Task 2), `clock_trust::wall_exceeds_forward_skew` (for `received_at`), `clock_trust::clamp_wall_to_forward_skew` (for `dismissed_at`).
- Fields: `dismissed_received_grants: BTreeMap<[u8;32], u64>`; `ReceivedFileGrant.received_at: u64`.

- [ ] **Step 1: Write the failing discrimination test.** Local holds a **dismissed** received-grant for CID at real `now`: `received_file_grants[cid]` with `received_at < now`, and `dismissed_received_grants[cid] = now` (so it is currently dismissed — inactive). Remote re-supplies the same CID with `received_at = now + 400 days`. After `merge_remote_into_local`, assert the grant is **still dismissed** — the final sweep drops it, so `local.received_file_grants.get(cid)` is `None` (the poisoned re-supply was rejected, so `received_at (old, < now)` never rose above `dismissed_at (now)`). Without the reject, the poisoned `received_at` wins the union/`active()` tie-break and survives the sweep (leak — grant reactivated). Add an in-window companion: a legit re-supply at `received_at = now + 1000` (in-window) after an *older* dismiss **does** reactivate (survives the sweep), proving the guard isn't over-rejecting.

- [ ] **Step 2: Run to verify it fails** (poisoned re-supply survives; grant reads active/present).

- [ ] **Step 3: Add the guards.** Dismiss-tombstone loop (512) — clamp the SAFE direction:

```rust
    for (cid, dismissed_at) in dismissed_received_grants {
        // ZEB-847 (RG): `dismissed_at` is the safe (deactivating) direction;
        // clamp its magnitude so a future dismiss can't pin a received-grant
        // dismissed out of reach of a legit re-share (grief-lockout).
        let dismissed_at = crate::clock_trust::clamp_wall_to_forward_skew(dismissed_at, receiver_now);
        let slot = local.dismissed_received_grants.entry(cid).or_insert(0);
        *slot = (*slot).max(dismissed_at);
    }
```

Received-grant union loop (535) — reject the FAIL-OPEN direction before the `active()` tie-break and any insert:

```rust
    for (cid, grant) in received_file_grants {
        // ZEB-847 (RG, CRITICAL vector): `received_at` is the FAIL-OPEN direction
        // — a future value wins the union/`active()` tie-break and survives the
        // dismiss sweep (activeness is `received_at > dismissed_at`, a static
        // max-merged compare). A clamp is insufficient (clamped now+5min still
        // beats an honest dismiss at now), so REJECT — mirrors GR (Task 5). A
        // legit re-share (received_at at real now, past receiver_now) is accepted.
        if crate::clock_trust::wall_exceeds_forward_skew(grant.received_at, receiver_now) {
            continue;
        }
        match local.received_file_grants.get(&cid) {
            // ... unchanged body ...
        }
    }
```

(Leave the final ZEB-727 sweep at 570-575 unchanged — it reads the stored values; the rejected re-supply simply never entered.)

- [ ] **Step 4: Run to verify both tests pass.** (Poisoned re-supply rejected → swept out → dismiss stands; in-window re-supply accepted → survives the sweep → reactivates.)

- [ ] **Step 5: Commit** — `-m "ZEB-847: reject future-dated received-grant received_at, clamp dismissed_at at merge (RG)"`

---

## Task 7: notes `merge_from` reject (C9)

Finding C9 (`notes_crdt.rs:57`): notes merge through a **separate** engine (`NotesDoc::merge_from`, `notes_crdt.rs:91`), per-id LWW by `updated_at`. A future `updated_at` causes silent note data loss / suppresses a legitimate later edit. `merge_from(&mut self, remote)` is ambient — sample `receiver_now_ms()` at its top and **reject** a future `r.updated_at`.

**Files:**
- Modify: `src-tauri/src/notes_crdt.rs` (`merge_from`, ~line 91)
- Test: `notes_crdt.rs` `#[cfg(test)]` module (co-locate with existing notes tests)

**Interfaces:**
- Consumes: `crate::clock_trust::{receiver_now_ms, wall_exceeds_forward_skew}`.
- Field: `Note.updated_at: Hlc` → `.wall_ms`.

- [ ] **Step 1: Write the failing test.** Local `NotesDoc` holds an honest edit of note N at real `now`. Remote `NotesDoc` holds the same note with different content at `updated_at = now + 400 days`. After `local.merge_from(remote)`, assert local still has the honest content (the future edit was rejected). Add an in-window companion (`now + 1000` merges normally).

- [ ] **Step 2: Run to verify it fails.**

- [ ] **Step 3: Add the sample point + reject** in `merge_from`. At the top of the function:

```rust
        // ZEB-847 (C9): bound each remote note's `updated_at` against the
        // receiver's own clock; a future stamp would LWW-win and silently drop
        // a legitimate local edit. Sampled once for the whole merge; `None`
        // (unreadable clock) ⇒ apply-all.
        let receiver_now = crate::clock_trust::receiver_now_ms();
```

Then in the per-id LWW loop, before the `is_strictly_newer_than` accept/insert, skip future entries:

```rust
            if crate::clock_trust::wall_exceeds_forward_skew(r.updated_at.wall_ms, receiver_now) {
                continue;
            }
```

(Place the guard so it also covers the `deleted_at` path — since a delete bumps `updated_at`, bounding `updated_at` covers both. Verify against the actual loop structure at 91-109.)

- [ ] **Step 4: Run to verify both tests pass.**

- [ ] **Step 5: Commit** — `-m "ZEB-847: reject future-dated note updated_at at NotesDoc merge (C9)"`

---

## Task 8: Existing-test sweep + full CI-parity gate

A new forward bound at the merge boundary can retroactively break existing owner-state / notes merge tests that construct entries with wall stamps far from the real present (hardcoded small epochs, or deliberately large future values for LWW ordering). Those entries will now be *rejected/clamped* by the guards, changing merge outcomes and failing assertions that predate this bound (the ZEB-831/846 "wall-clock gate retroactively breaks real-clock tests" hazard).

**Files:**
- Modify (as needed): any owner-state / notes merge test that trips the new bound.

- [ ] **Step 1: Enumerate at-risk tests.** From `src-tauri/`:
  - `grep -rn "merge_remote_into_local\|merge_from" src/ tests/` — every merge caller in tests.
  - In those tests, find entries whose `wall_ms` / `granted_at` / `revoked_at` / `received_at` / `dismissed_at` is a small constant (e.g. `1`, `1000`, epoch-ish) **or** a large hardcoded future value. A small constant is > 5 min behind *real* `now`, so it is *accepted* (past is fine) — the risk is only stamps set **more than 5 min ahead of real now**: hardcoded far-future values used to force LWW ordering, and any test that builds "newer" entries as `now + large`.
- [ ] **Step 2: Fix each tripped test** by pinning its stamps to a realistic present (`SystemTime::now()`-derived ms, with in-window offsets for "newer") instead of an unbounded future constant — so the test exercises real LWW ordering *within* the tolerance window. Do not weaken the new guards to accommodate a test; the guard is the spec. Backdating a small-constant "older" stamp is fine (past is always accepted).
- [ ] **Step 3: Run the full CI-parity sweep** (not `test-select`) from `src-tauri/`:
  - `cargo fmt --all -- --check`
  - `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  - `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
  - Capture pass/fail counts and real exit codes (`${pipestatus[1]}` if piping).
- [ ] **Step 4: Commit** any test fixes — `-m "ZEB-847: pin owner-state merge tests to in-window walls under the new forward bound"` (skip the commit if the sweep is green with no test changes needed; record that in the task report).

---

## Self-Review

- **Spec coverage (ZEB-847 findings → tasks):** FR→T2, SP→T3, DEV/LE/RM→T4, GR→T5, RG→T6, C9(notes)→T7. All eight owner-state merge sites from the ticket are covered. The read-marker (RM, LOW) and notes (C9) sites named in ZEB-847's "notes / read markers" bullet are both included.
- **Reject-vs-clamp sign:** HLC-LWW-replace sites reject (T2,T3,T4,T7); grow-only `u64` `max`-join sites clamp (T5,T6). Consistent with the Global Constraint and the spec §6.2.
- **Sample-once:** `receiver_now` bound once in `merge_remote_into_local` (T2) and once in `merge_from` (T7); reused, never re-sampled.
- **`None`⇒apply-all:** centralized in the Task-1 helpers; no site substitutes `0`.
- **No write-back / forward-only:** guards `continue`/`clamp` in place; no store rewrite; no backdating guard added; Space `created_at` untouched.
- **Type consistency:** `wall_exceeds_forward_skew(u64, Option<u64>) -> bool` and `clamp_wall_to_forward_skew(u64, Option<u64>) -> u64` are defined in T1 and consumed unchanged in T2–T7. Field types (`Hlc.wall_ms: u64`, grant `u64` stamps) match the recon.
- **Test hazard:** T8 sweeps existing tests for stamps the new bound would break — the known ZEB-831/846 regression vector — and runs the full CI-parity gate as the backstop.
