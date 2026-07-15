# ZEB-685 tail — RevocationPush durability rung + bounded revoked-device store — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bound the grow-only `revoked_dm_devices` store (ZEB-692) and give `RevocationPush` a butler-hold durability rung so an offline DM-only friend is reached automatically on reconnect (ZEB-691), in one bundled PR.

**Architecture:** Part A adds two convergent-under-union bounds to the owner-state CRDT store. Part B routes a device revocation through the butler deposit rung (`DmInboxDoc` = the friend's own always-on fleet): an additive `revocation_push` field on the deposit wire types, a shared cert-verify core, a butler-acceptor arm that pre-validates and persists under a synthetic key, a recipient inbox-sweeper arm that re-verifies + applies + marks owner-state dirty, and a send-side butler deposit.

**Tech Stack:** Rust (tauri app crate `harmony-app`), `cargo nextest`, serde/CBOR wire types, ed25519 certs (`harmony_owner::certs`).

Design doc: `docs/specs/2026-07-15-zeb-691-692-revocation-durability-store-bound-design.md`.

## Global Constraints

- **MSRV 1.91 / toolchain 1.94.1** — `BTreeSet::pop_last` (stable 1.66) is available.
- **`revoked_dm_devices` is union-merged, NOT LWW** — every bound is a *deterministic function of the merged set* (never a one-shot delete), so it converges across the owner's devices.
- **Additive wire fields only** — new `Option<Vec<u8>>` fields use `#[serde(rename = "…", default, skip_serializing_if = "Option::is_none", with = "serde_bytes")]`. Legacy blobs/snapshots decode the absent key to `None`.
- **Never trust the carrier** — the butler pre-validates a revocation before persist (D7), and the recipient re-verifies fully via `handle_revocation_push` on recover. The butler is not the recipient.
- **`notify_dirty` on a genuine local insert** — the recover path marks owner-state dirty iff `handle_revocation_push` returns `Ok(true)`; a deposited revocation entry is eventually GC'd, so there is no re-delivery backstop.
- Gates (run from `src-tauri/`): `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --all-targets --features test-fixtures`; MSRV `cargo check --locked --all-targets --features test-fixtures`. Frontend gates unaffected (no frontend change).
- All `cargo`/`nextest` commands run from `/Users/zeblith/work/zeblithic/harmony-client/src-tauri`.

---

# Part A — ZEB-692: bound `revoked_dm_devices`

### Task A1: Per-owner cap in `apply_revoked_dm_device`

**Files:**
- Modify: `src-tauri/src/owner_state_crdt.rs` (const near the top of the impl module; `apply_revoked_dm_device` at ~905)
- Test: same file (`#[cfg(test)]` module, near the existing `apply_revoked_dm_device_unions` at ~4259)

**Interfaces:**
- Produces: `pub const MAX_REVOKED_DM_DEVICES_PER_OWNER: usize = 256;`
- `apply_revoked_dm_device(owner, ed25519) -> bool` keeps its signature; the returned `bool` now means "a new, *retained* key was added" (drives `notify_dirty`).

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)]` module in `owner_state_crdt.rs`:

```rust
#[test]
fn apply_revoked_dm_device_caps_at_max_keeping_smallest() {
    let mut s = OwnerState::default();
    let owner = crate::owner_state_types::OwnerAddr([0x11; 16]);
    // Insert MAX distinct keys 1..=MAX (byte-encoded), all retained.
    for i in 0..MAX_REVOKED_DM_DEVICES_PER_OWNER {
        let mut ed = [0u8; 32];
        ed[0] = ((i >> 8) & 0xff) as u8;
        ed[1] = (i & 0xff) as u8;
        assert!(s.apply_revoked_dm_device(owner, ed), "fresh key retained");
    }
    assert_eq!(
        s.revoked_dm_devices.get(&owner).unwrap().len(),
        MAX_REVOKED_DM_DEVICES_PER_OWNER
    );
    // A key GREATER than the current max is inserted-then-evicted → no net change → false.
    let big = [0xff; 32];
    assert!(!s.apply_revoked_dm_device(owner, big), "over-cap larger key not retained");
    assert!(!s.revoked_dm_devices.get(&owner).unwrap().contains(&big));
    assert_eq!(
        s.revoked_dm_devices.get(&owner).unwrap().len(),
        MAX_REVOKED_DM_DEVICES_PER_OWNER
    );
    // A key SMALLER than the current max evicts the max → net change → true.
    let small = [0u8; 32]; // 0x0000… is smaller than any two-byte-tagged key above except itself
    let was_present = s.revoked_dm_devices.get(&owner).unwrap().contains(&small);
    let ret = s.apply_revoked_dm_device(owner, small);
    assert_eq!(ret, !was_present, "small key retained iff it was new");
    assert!(s.revoked_dm_devices.get(&owner).unwrap().contains(&small));
    assert_eq!(
        s.revoked_dm_devices.get(&owner).unwrap().len(),
        MAX_REVOKED_DM_DEVICES_PER_OWNER
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(apply_revoked_dm_device_caps_at_max_keeping_smallest)'`
Expected: FAIL — the current `apply_revoked_dm_device` has no cap, so the over-cap `big` insert returns `true` and grows the set past `MAX`.

- [ ] **Step 3: Add the const and cap logic**

Add the const above the `impl OwnerState` block (near other `pub const` items in the file):

```rust
/// ZEB-692: hard cap on the number of revoked #2 ed25519 keys retained per
/// friend `OwnerAddr` in `revoked_dm_devices`. A real fleet is single-digit
/// devices; 256 is a generous DoS backstop against a friend minting + revoking
/// many synthetic devices. Enforced as "keep the smallest-N by byte order"
/// (deterministic ⇒ convergent under the union merge — see
/// `owner_state_sync::merge_remote_into_local`).
pub const MAX_REVOKED_DM_DEVICES_PER_OWNER: usize = 256;
```

Replace `apply_revoked_dm_device` (currently at ~905) with:

```rust
pub fn apply_revoked_dm_device(
    &mut self,
    owner: crate::owner_state_types::OwnerAddr,
    ed25519: [u8; 32],
) -> bool {
    let set = self.revoked_dm_devices.entry(owner).or_default();
    let was_new = set.insert(ed25519);
    // ZEB-692: keep the smallest-N by byte order. `pop_last` removes the
    // greatest; a deterministic set→set function, so every device converges to
    // the same capped set under the union merge. If the just-inserted key was
    // itself the evicted max, the store is unchanged → report no net change so
    // the caller does not spuriously `notify_dirty`.
    while set.len() > MAX_REVOKED_DM_DEVICES_PER_OWNER {
        set.pop_last();
    }
    was_new && set.contains(&ed25519)
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(apply_revoked_dm_device)'`
Expected: PASS (both the new cap test and the existing `apply_revoked_dm_device_unions`).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/owner_state_crdt.rs
git commit -m "ZEB-692: cap revoked_dm_devices per owner (smallest-N, convergent)"
```

---

### Task A2: Cap + status-prune on the merge path

**Files:**
- Modify: `src-tauri/src/owner_state_sync.rs` (`merge_remote_into_local` union loop at ~370-376)
- Test: same file (`#[cfg(test)]`, near `revoked_dm_devices_merge_is_union_not_lww` at ~2041)

**Interfaces:**
- Consumes: `MAX_REVOKED_DM_DEVICES_PER_OWNER` (Task A1), `FriendStatus::Revoked`.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)]` module in `owner_state_sync.rs`:

```rust
#[test]
fn merge_caps_revoked_dm_devices_to_smallest_n_and_converges() {
    use crate::owner_state_crdt::{OwnerState, MAX_REVOKED_DM_DEVICES_PER_OWNER};
    let owner = crate::owner_state_types::OwnerAddr([0x22; 16]);
    let mk = |base: u8| {
        let mut s = OwnerState::default();
        for i in 0..MAX_REVOKED_DM_DEVICES_PER_OWNER {
            let mut ed = [0u8; 32];
            ed[0] = base;
            ed[1] = ((i >> 8) & 0xff) as u8;
            ed[2] = (i & 0xff) as u8;
            s.apply_revoked_dm_device(owner, ed);
        }
        s
    };
    let mut a = mk(0x00);
    let b = mk(0x01); // disjoint key space (base byte differs)
    merge_remote_into_local(&mut a, b.clone());
    assert_eq!(
        a.revoked_dm_devices.get(&owner).unwrap().len(),
        MAX_REVOKED_DM_DEVICES_PER_OWNER,
        "union capped back to N"
    );
    // Convergence: merging b again is a no-op (already the N-smallest of a∪b).
    let before = a.revoked_dm_devices.clone();
    merge_remote_into_local(&mut a, b);
    assert_eq!(a.revoked_dm_devices, before, "re-merge is idempotent");
}

#[test]
fn merge_prunes_revoked_dm_devices_for_revoked_friends() {
    use crate::friend_graph::{FriendEntry, FriendOrigin, FriendStatus};
    use crate::owner_state_crdt::OwnerState;
    // A friend we hold a revoked-device entry for, whose friendship the remote
    // snapshot has just tombstoned (Revoked). The merge must drop the entry.
    let friend_master = [7u8; 32];
    let friend_addr = crate::friend_graph::owner_id_from_master_ed25519(&friend_master);
    let mut local = OwnerState::default();
    local.apply_revoked_dm_device(friend_addr, [9u8; 32]);
    // Local still thinks the friend is Active.
    local.apply_friend_update(
        friend_addr,
        FriendEntry {
            master_ed25519: friend_master,
            display: None,
            status: FriendStatus::Active,
            established_via: FriendOrigin::Token,
            referrable: false,
            learned_at: crate::owner_state_types::Hlc { wall_ms: 1, logical: 0, device_id: "x".into() },
            sealed_secret: None,
        },
    );
    // Remote snapshot carries a strictly-newer Revoked tombstone.
    let mut remote = OwnerState::default();
    remote.apply_friend_update(
        friend_addr,
        FriendEntry {
            master_ed25519: friend_master,
            display: None,
            status: FriendStatus::Revoked,
            established_via: FriendOrigin::Token,
            referrable: false,
            learned_at: crate::owner_state_types::Hlc { wall_ms: 2, logical: 0, device_id: "x".into() },
            sealed_secret: None,
        },
    );
    merge_remote_into_local(&mut local, remote);
    assert_eq!(
        local.friend_graph.friends[&friend_addr].status,
        FriendStatus::Revoked
    );
    assert!(
        !local.revoked_dm_devices.contains_key(&friend_addr),
        "revoked-device entry pruned for a de-friended owner"
    );
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(merge_caps_revoked_dm_devices_to_smallest_n_and_converges) | test(merge_prunes_revoked_dm_devices_for_revoked_friends)'`
Expected: FAIL — the union has no cap, and no prune of Revoked-owner entries.

- [ ] **Step 3: Add cap + prune after the union loop**

In `merge_remote_into_local`, replace the existing `revoked_dm_devices` union loop (currently at ~370-376) with:

```rust
// ZEB-685 (S3): friend-scoped DM revocations are GROW-ONLY — union per owner
// key (mirrors `apply_outbox`'s `delivered_to.extend`). NOT LWW: two of the
// owner's own devices each learning a different revocation must both survive.
// ZEB-692: after the union, re-apply the two convergent bounds so a sibling
// snapshot cannot re-inflate past them —
//   (a) cap each touched owner's set to the smallest-N by byte order;
//   (b) prune the set for any owner whose merged friend status is `Revoked`
//       (a de-friended contact's DM cutoff is moot). friend_graph is merged
//       ABOVE this loop, so the status is already converged here.
for (owner, set) in revoked_dm_devices {
    let local_set = local.revoked_dm_devices.entry(owner).or_default();
    local_set.extend(set);
    while local_set.len() > crate::owner_state_crdt::MAX_REVOKED_DM_DEVICES_PER_OWNER {
        local_set.pop_last();
    }
}
// GC-on-de-friend (convergent prune): drop entries whose owner is present in the
// merged friend graph AS `Revoked`. Runs over the whole store (not just touched
// keys) so a Revoked tombstone that arrived in THIS merge also cleans a
// pre-existing local entry.
local.revoked_dm_devices.retain(|owner, _| {
    !matches!(
        local.friend_graph.friends.get(owner).map(|e| &e.status),
        Some(crate::friend_graph::FriendStatus::Revoked)
    )
});
```

Note: the `revoked_dm_devices` binding is the destructured remote field (already in scope from the `let OwnerState { … revoked_dm_devices, … } = remote;` destructure at ~236). Confirm the `friend_graph.friends` merge (the `for (addr, entry) in friend_graph.friends` loop at ~353) still runs BEFORE this block — it does; leave its position unchanged.

- [ ] **Step 4: Run to verify pass**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(merge_caps_revoked_dm_devices) | test(merge_prunes_revoked_dm_devices) | test(revoked_dm_devices_merge_is_union_not_lww)'`
Expected: PASS (new tests + the existing union test still green — the union semantics are unchanged below the cap).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/owner_state_sync.rs
git commit -m "ZEB-692: cap + de-friend prune revoked_dm_devices on the merge path"
```

---

### Task A3: GC-on-de-friend in `unfriend_inner`

**Files:**
- Modify: `src-tauri/src/lib.rs` (`unfriend_inner` at ~52972; the apply at ~53016)
- Test: same file (`#[cfg(test)]`, near `unfriend_inner_tombstones_active_then_list_hides_it` at ~55584)

**Interfaces:**
- Consumes: `unfriend_inner` writes a `Revoked` tombstone; add the local revoked-set drop under the same lock.

- [ ] **Step 1: Write the failing test**

Add near the existing `unfriend_inner` tests in `lib.rs`:

```rust
#[tokio::test]
async fn unfriend_inner_drops_local_revoked_dm_devices_for_peer() {
    use crate::friend_graph::{FriendEntry, FriendOrigin, FriendStatus};
    let friend_master = [5u8; 32];
    let peer = crate::friend_graph::owner_id_from_master_ed25519(&friend_master);
    let crdt_state = std::sync::Arc::new(tokio::sync::Mutex::new(
        crate::owner_state_crdt::OwnerState::default(),
    ));
    let hlc_tracker = std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::BTreeMap::new()));
    {
        let mut s = crdt_state.lock().await;
        s.apply_friend_update(
            peer,
            FriendEntry {
                master_ed25519: friend_master,
                display: None,
                status: FriendStatus::Active,
                established_via: FriendOrigin::Token,
                referrable: false,
                learned_at: crate::owner_state_types::Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
                sealed_secret: None,
            },
        );
        s.apply_revoked_dm_device(peer, [8u8; 32]);
        assert!(s.revoked_dm_devices.contains_key(&peer));
    }
    let changed = unfriend_inner(&crdt_state, &hlc_tracker, "d", peer)
        .await
        .expect("unfriend ok");
    assert!(changed);
    let s = crdt_state.lock().await;
    assert_eq!(s.friend_graph.friends[&peer].status, FriendStatus::Revoked);
    assert!(
        !s.revoked_dm_devices.contains_key(&peer),
        "revoked-device entry GC'd on de-friend"
    );
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(unfriend_inner_drops_local_revoked_dm_devices_for_peer)'`
Expected: FAIL — `unfriend_inner` does not touch `revoked_dm_devices`.

- [ ] **Step 3: Drop the local entry on the Revoked transition**

In `unfriend_inner`, the tail acquires the lock and applies the tombstone:

```rust
    let mut state = crdt_state.lock().await;
    match state.apply_friend_update(peer_addr, tombstone) {
        crate::owner_state_crdt::ApplyOutcome::Inserted
        | crate::owner_state_crdt::ApplyOutcome::Merged { .. } => Ok(true),
        crate::owner_state_crdt::ApplyOutcome::Rejected(reason) => {
            Err(format!("unfriend apply rejected: {reason:?}"))
        }
    }
```

Change the success arm to also GC the store (still under the held lock):

```rust
    let mut state = crdt_state.lock().await;
    match state.apply_friend_update(peer_addr, tombstone) {
        crate::owner_state_crdt::ApplyOutcome::Inserted
        | crate::owner_state_crdt::ApplyOutcome::Merged { .. } => {
            // ZEB-692: a de-friended contact's DM cutoff is moot — drop their
            // revoked-device set locally. The merge-path prune (Task A2) keeps
            // this from being re-inflated by a sibling that has not yet seen the
            // Revoked tombstone, so this is just the immediate local free.
            state.revoked_dm_devices.remove(&peer_addr);
            Ok(true)
        }
        crate::owner_state_crdt::ApplyOutcome::Rejected(reason) => {
            Err(format!("unfriend apply rejected: {reason:?}"))
        }
    }
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(unfriend_inner)'`
Expected: PASS (new test + existing `unfriend_inner_*` tests).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "ZEB-692: GC revoked_dm_devices on de-friend (unfriend_inner)"
```

---

# Part B — ZEB-691: butler-hold durability rung

### Task B1: `DmInboxEntry.revocation_push` field + `DmInboxDoc::revoke_key`

**Files:**
- Modify: `src-tauri/src/dm_inbox_crdt.rs` (`DmInboxEntry` struct; add `revoke_key`)
- Modify (constructors to keep compiling): `src-tauri/src/iroh_butler_acceptor.rs:799`, `src-tauri/src/dm_inbox_persist.rs:241`, any `#[cfg(test)]` `DmInboxEntry { … }` literals (grep below)
- Test: `src-tauri/src/dm_inbox_crdt.rs` (`#[cfg(test)]`)

**Interfaces:**
- Produces: `DmInboxEntry.revocation_push: Option<Vec<u8>>` (serde `rp`); `DmInboxDoc::revoke_key(revoked_owner: &[u8;16], revoked_target: &[u8;16]) -> String`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn revoke_key_de_collides_with_message_and_invite_keys() {
    let space = [0xAB; 16];
    let cid = [0xCD; 32];
    let owner = [0x11; 16];
    let device = [0x22; 16];
    let msg = DmInboxDoc::key(&space, &cid);
    let inv = DmInboxDoc::invite_key(&space);
    let rev = DmInboxDoc::revoke_key(&owner, &device);
    assert!(rev.starts_with("revoke:"));
    assert_ne!(rev, msg);
    assert_ne!(rev, inv);
    // A revoke key's first segment is the literal "revoke", never 32 hex chars,
    // so it can never alias a space-scoped key.
    assert!(!msg.starts_with("revoke:"));
    assert!(!inv.starts_with("revoke:"));
}

#[test]
fn dm_inbox_entry_round_trips_revocation_push() {
    let e = DmInboxEntry {
        sender_owner: [1u8; 16],
        cidnotify_packet: None,
        storage_blob: Vec::new(),
        invite_packet: None,
        revocation_push: Some(vec![0x05, 0xAA, 0xBB]),
        deposited_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        deposited_by: "d".into(),
        ingested_by: Default::default(),
    };
    let bytes = crate::owner_state_crypto::canonical_cbor_encode(&e).unwrap();
    let back: DmInboxEntry = crate::owner_state_crypto::canonical_cbor_decode(&bytes).unwrap();
    assert_eq!(back.revocation_push, Some(vec![0x05, 0xAA, 0xBB]));
}
```

(If the file already has a canonical-CBOR round-trip helper for `DmInboxEntry`, mirror its exact encode/decode calls instead of the ones above.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(revoke_key_de_collides) | test(dm_inbox_entry_round_trips_revocation_push)'`
Expected: FAIL — `revoke_key` and the `revocation_push` field don't exist (compile error).

- [ ] **Step 3: Add the field and the key helper**

In `DmInboxEntry` (after the `invite_packet` field), add:

```rust
    /// ZEB-691: signed `RevocationPush` frame bytes (a `DmPacket::RevocationPush`),
    /// carried through from the sealed `DepositPayload` by the butler acceptor.
    /// Applied on recover via `handle_revocation_push`. `None` for message /
    /// invite / legacy deposits.
    #[serde(
        rename = "rp",
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_bytes"
    )]
    pub revocation_push: Option<Vec<u8>>,
```

Add the key helper next to `key` / `invite_key`:

```rust
/// ZEB-691: deposit key for a standalone device-revocation entry (no message,
/// no space). Keyed by the revoking friend's owner + the revoked device id, so
/// re-depositing the same revocation is idempotent (one entry per revoked
/// device). The literal `revoke` first segment can never be 32 hex chars, so it
/// cannot collide with a message key (`{space_hex}:{cid_hex}`) or an invite key
/// (`{space_hex}:invite`).
pub fn revoke_key(revoked_owner: &[u8; 16], revoked_target: &[u8; 16]) -> String {
    format!("revoke:{}:{}", hex::encode(revoked_owner), hex::encode(revoked_target))
}
```

- [ ] **Step 4: Fix all `DmInboxEntry` constructors**

Run: `grep -rn "DmInboxEntry {" src-tauri/src` and add `revocation_push: None,` to every literal (production: `iroh_butler_acceptor.rs:799`, `dm_inbox_persist.rs:241`; plus any test literals). The butler-acceptor one at `:799` will be revisited in Task B4 to carry `payload.revocation_push` — set it to `None` here so the tree compiles.

Then run: `cargo nextest run --locked --features test-fixtures -E 'test(revoke_key_de_collides) | test(dm_inbox_entry_round_trips_revocation_push)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/dm_inbox_crdt.rs src-tauri/src/iroh_butler_acceptor.rs src-tauri/src/dm_inbox_persist.rs
git commit -m "ZEB-691: DmInboxEntry.revocation_push field + DmInboxDoc::revoke_key"
```

---

### Task B2: `DepositPayload` / `ButlerDepositRequest` fields + `REVOCATION_DEPOSIT_MARKER`

**Files:**
- Modify: `src-tauri/src/butler_deposit.rs` (`DepositPayload` at ~196; `ButlerDepositRequest`; a new marker const near `INVITE_ONLY_DEPOSIT_MARKER` at ~62; `IrohButlerDepositClient::deposit` `expect_cid` + payload build)
- Modify (constructors): `src-tauri/src/community_relay_prod.rs:1038` (`DepositPayload { … }`), plus any other `DepositPayload { … }` literals (grep)
- Test: `src-tauri/src/butler_deposit.rs` (`#[cfg(test)]`, near `deposit_payload_round_trips` at ~780)

**Interfaces:**
- Produces: `DepositPayload.revocation_push: Option<Vec<u8>>` (serde `rp`); `ButlerDepositRequest.revocation_push: Option<Vec<u8>>`; `pub const REVOCATION_DEPOSIT_MARKER: &[u8] = b"zeb691-revocation";`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn deposit_payload_round_trips_revocation_push_and_decodes_legacy_as_none() {
    let with_rev = DepositPayload {
        cidnotify_packet: None,
        storage_blob: Vec::new(),
        invite_packet: None,
        revocation_push: Some(vec![0x05, 1, 2, 3]),
    };
    let bytes = encode_deposit_payload(&with_rev).unwrap();
    assert_eq!(decode_deposit_payload(&bytes).unwrap(), with_rev);

    // Legacy payload (no `rp`) decodes revocation_push to None.
    let legacy = DepositPayload {
        cidnotify_packet: Some(vec![9]),
        storage_blob: vec![1],
        invite_packet: None,
        revocation_push: None,
    };
    let lbytes = encode_deposit_payload(&legacy).unwrap();
    assert_eq!(decode_deposit_payload(&lbytes).unwrap().revocation_push, None);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(deposit_payload_round_trips_revocation_push_and_decodes_legacy_as_none)'`
Expected: FAIL — the field doesn't exist (compile error).

- [ ] **Step 3: Add the fields, marker, and client branch**

Add the marker near `INVITE_ONLY_DEPOSIT_MARKER`:

```rust
/// ZEB-691: ack marker returned for a device-revocation deposit (no message
/// CID). Mirrors `INVITE_ONLY_DEPOSIT_MARKER`; the sender's `IrohButlerDepositClient`
/// binds the ack to this value for a `revocation_push` request.
pub const REVOCATION_DEPOSIT_MARKER: &[u8] = b"zeb691-revocation";
```

Add to `DepositPayload` (after `invite_packet`):

```rust
    /// ZEB-691: signed `RevocationPush` frame bytes for the recipient's inbox
    /// sweeper to apply via `handle_revocation_push`. `None` for message /
    /// invite / legacy deposits.
    #[serde(
        rename = "rp",
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_bytes"
    )]
    pub revocation_push: Option<Vec<u8>>,
```

Add to `ButlerDepositRequest` (after `invite_packet`):

```rust
    /// ZEB-691: signed `RevocationPush` frame bytes. When `Some`, this is a
    /// revocation deposit — `cidnotify_packet`/`invite_packet`/`message_cid` are
    /// all `None` and the ack binds to `REVOCATION_DEPOSIT_MARKER`.
    pub revocation_push: Option<Vec<u8>>,
```

In `IrohButlerDepositClient::deposit`, the `expect_cid` computation currently is:

```rust
        let (storage_blob, expect_cid): (Vec<u8>, Vec<u8>) = match req.message_cid {
            Some(message_cid) => { /* fetch blob */ (blob, message_cid.to_bytes().to_vec()) }
            None => (Vec::new(), INVITE_ONLY_DEPOSIT_MARKER.to_vec()),
        };
        let payload = DepositPayload {
            cidnotify_packet: req.cidnotify_packet.clone(),
            storage_blob,
            invite_packet: req.invite_packet.clone(),
        };
```

Change the `None` arm to distinguish a revocation, and carry the field into the payload:

```rust
        let (storage_blob, expect_cid): (Vec<u8>, Vec<u8>) = match req.message_cid {
            Some(message_cid) => {
                let blob = match self.cas.get(&message_cid).await {
                    Ok(Some(blob)) => blob,
                    Ok(None) => return DepositRungOutcome::Failed("storage blob missing from CAS".to_string()),
                    Err(e) => return DepositRungOutcome::Failed(format!("CAS get: {e}")),
                };
                (blob, message_cid.to_bytes().to_vec())
            }
            // ZEB-691: a revocation deposit has no message CID and its own ack
            // marker; an invite-only deposit keeps the ZEB-505 marker.
            None if req.revocation_push.is_some() => {
                (Vec::new(), crate::butler_deposit::REVOCATION_DEPOSIT_MARKER.to_vec())
            }
            None => (Vec::new(), INVITE_ONLY_DEPOSIT_MARKER.to_vec()),
        };
        let payload = DepositPayload {
            cidnotify_packet: req.cidnotify_packet.clone(),
            storage_blob,
            invite_packet: req.invite_packet.clone(),
            revocation_push: req.revocation_push.clone(),
        };
```

(Keep the surrounding lines — the exact fetch code above mirrors what's already there; preserve any existing comments.)

- [ ] **Step 4: Fix all `DepositPayload` constructors + run**

Run: `grep -rn "DepositPayload {" src-tauri/src` and add `revocation_push: None,` to every literal that doesn't already set it (production: `community_relay_prod.rs:1038`; tests in `butler_deposit.rs`, `community_relay_prod.rs`, `dm_inbox_persist.rs`). The `community_relay_prod.rs:1038` relay-client payload build is on the unused-for-revocation relay path, so `revocation_push: None` there is correct.

Then run: `cargo nextest run --locked --features test-fixtures -E 'test(deposit_payload_round_trips)'`
Expected: PASS (new + existing `deposit_payload_round_trips*`).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/butler_deposit.rs src-tauri/src/community_relay_prod.rs src-tauri/src/dm_inbox_persist.rs
git commit -m "ZEB-691: revocation_push on DepositPayload/ButlerDepositRequest + ack marker + client binding"
```

---

### Task B3: Extract `verify_revocation_push` from `handle_revocation_push`

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` (`handle_revocation_push` at ~2412)
- Test: same file (`#[cfg(test)]`, near the existing `handle_revocation_push_*` tests at ~3608)

**Interfaces:**
- Produces: `pub(crate) fn verify_revocation_push(expected_owner: OwnerAddr, revocation: &RevocationCert, enrollment: &EnrollmentCert) -> Result<[u8; 32], DmReceiveError>` — steps 1–3 (verify + trust-bind + target-bind), returns the bridged ed25519.
- `handle_revocation_push` keeps its signature and behaviour; it now calls the extracted core then does the store + projection.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn verify_revocation_push_accepts_valid_and_rejects_tampered() {
    let case = sample_revocation_case(); // existing test helper (RevCase)
    let ed = verify_revocation_push(case.owner, &case.revocation, &case.enrollment)
        .expect("valid pair verifies");
    assert_eq!(ed, case.revoked_ed);
    // Third-party owner (expected != revocation.owner) → OwnerFieldMismatch.
    let other = crate::owner_state_types::OwnerAddr([0xEE; 16]);
    assert!(matches!(
        verify_revocation_push(other, &case.revocation, &case.enrollment),
        Err(DmReceiveError::OwnerFieldMismatch)
    ));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(verify_revocation_push_accepts_valid_and_rejects_tampered)'`
Expected: FAIL — `verify_revocation_push` doesn't exist.

- [ ] **Step 3: Extract the core**

Add above `handle_revocation_push`:

```rust
/// ZEB-691: the cert-verification + trust-bind core of `handle_revocation_push`,
/// factored out so the butler acceptor can PRE-VALIDATE a deposited revocation
/// (D7: never persist+ack a forgery) with the SAME authority the recipient uses
/// on recover. Returns the bridged revoked #2 ed25519 verify key.
pub(crate) fn verify_revocation_push(
    expected_owner: OwnerAddr,
    revocation: &harmony_owner::certs::RevocationCert,
    enrollment: &harmony_owner::certs::EnrollmentCert,
) -> Result<[u8; 32], DmReceiveError> {
    // 1. Master-signed revocation — `verify(None)` self-verifies the embedded
    //    master pub and binds `master.identity_hash() == revocation.owner_id`.
    revocation
        .verify(None)
        .map_err(|_| DmReceiveError::SignatureVerificationFailed)?;
    // 2. Trust-bind: the revocation AND the paired enrollment must belong to the
    //    pushing friend — a friend may only revoke THEIR OWN devices.
    if OwnerAddr(revocation.owner_id) != expected_owner
        || OwnerAddr(enrollment.owner_id) != expected_owner
    {
        return Err(DmReceiveError::OwnerFieldMismatch);
    }
    // 3. Verify the enrollment chain EXPIRY-AGNOSTIC (a revoked device may hold
    //    an expired cert — the sig + id-binding secure the target→ed25519
    //    bridge), then bind the enrollment to the cert's target.
    enrollment
        .verify(0)
        .map_err(|_| DmReceiveError::SignatureVerificationFailed)?;
    if enrollment.device_id != revocation.target {
        return Err(DmReceiveError::OwnerFieldMismatch);
    }
    Ok(enrollment.device_pubkeys.classical.ed25519_verify)
}
```

Replace the body of `handle_revocation_push` with:

```rust
pub fn handle_revocation_push(
    state: &mut OwnerState,
    expected_owner: OwnerAddr,
    revocation: &harmony_owner::certs::RevocationCert,
    enrollment: &harmony_owner::certs::EnrollmentCert,
    revoked: &crate::revoked_device_projection::RevokedDeviceProjection,
) -> Result<bool, DmReceiveError> {
    let ed25519 = verify_revocation_push(expected_owner, revocation, enrollment)?;
    // Store union-merged (survives across the owner's devices; capped ZEB-692)
    // and feed the live projection.
    let inserted = state.apply_revoked_dm_device(expected_owner, ed25519);
    let mut one = std::collections::BTreeSet::new();
    one.insert(ed25519);
    revoked.union_from_members(std::iter::once((expected_owner, &one)));
    Ok(inserted)
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(verify_revocation_push) | test(handle_revocation_push)'`
Expected: PASS — the new extract test and all four existing `handle_revocation_push_*` tests (behaviour is unchanged).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/dm_outbox.rs
git commit -m "ZEB-691: extract verify_revocation_push core from handle_revocation_push"
```

---

### Task B4: Butler acceptor revocation arm

**Files:**
- Modify: `src-tauri/src/iroh_butler_acceptor.rs` (`handle_deposit_core`, the `None =>` arm at ~723; the persisted `DmInboxEntry` at ~799)
- Test: same file (`#[cfg(test)]`)

**Interfaces:**
- Consumes: `verify_revocation_push` (B3), `DmInboxDoc::revoke_key` (B1), `REVOCATION_DEPOSIT_MARKER` (B2), `DepositPayload.revocation_push` (B2), `DmInboxEntry.revocation_push` (B1).

- [ ] **Step 1: Write the failing test**

Mirror the existing invite-only acceptor tests. Build a real master-signed `RevocationPush` (reuse the `sample_revocation_case`-style helper or `RecoveryArtifact::from_seed`/`sign_master`), seal a `DepositPayload { revocation_push: Some(wire), .. all None/empty }`, and assert the acceptor persists it under `revoke_key` and acks with `REVOCATION_DEPOSIT_MARKER`; assert a payload whose `revocation.owner_id != frame.sender_owner` is rejected without persisting; assert a non-empty `storage_blob` is rejected. Use the existing test harness/ctx in this file (`persist_entry` mock captures the key). Name it `handle_deposit_core_persists_revocation_under_revoke_key`, `handle_deposit_core_rejects_forged_revocation`, `handle_deposit_core_rejects_revocation_with_blob`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(handle_deposit_core_persists_revocation_under_revoke_key) | test(handle_deposit_core_rejects_forged_revocation) | test(handle_deposit_core_rejects_revocation_with_blob)'`
Expected: FAIL — no revocation arm; a revocation-only payload currently hits `BadPayload` (invite required).

- [ ] **Step 3: Add the revocation branch to the `None =>` arm**

At the TOP of the `None =>` arm (before the existing invite-only body), add:

```rust
            None => {
                // ZEB-691: a device-revocation deposit — no message, no invite,
                // a signed RevocationPush. Pre-validate the certs (D7: never
                // persist+ack a forgery) with the SAME authority the recipient
                // uses on recover, binding the revocation to the AUTHENTICATED
                // depositing friend (`frame.sender_owner`), and key by the
                // revoked device.
                if let Some(rp_bytes) = payload.revocation_push.as_deref() {
                    if rp_bytes.len() > crate::butler_deposit::MAX_DEPOSIT_INVITE_BYTES {
                        return Err(DepositReject::BadPayload);
                    }
                    if !payload.storage_blob.is_empty() {
                        return Err(DepositReject::BadPayload);
                    }
                    let packet = decode_packet(rp_bytes).map_err(|_| DepositReject::BadPayload)?;
                    let DmPacket::RevocationPush { revocation, enrollment } = packet else {
                        return Err(DepositReject::BadPayload);
                    };
                    crate::dm_outbox::verify_revocation_push(
                        crate::owner_state_types::OwnerAddr(frame.sender_owner),
                        &revocation,
                        &enrollment,
                    )
                    .map_err(|_| DepositReject::InnerVerifyFailed)?;
                    let key = DmInboxDoc::revoke_key(&frame.sender_owner, &revocation.target);
                    (
                        [0u8; 16],
                        key,
                        crate::butler_deposit::REVOCATION_DEPOSIT_MARKER.to_vec(),
                    )
                } else {
                    // … existing ZEB-505 invite-only body, unchanged …
                }
            }
```

Wrap the existing invite-only body in the `else { … }` (its final expression already yields the `(space, key, marker)` triple, so no other change is needed).

Then carry the field into the persisted entry (at ~799):

```rust
    let entry = DmInboxEntry {
        sender_owner: frame.sender_owner,
        cidnotify_packet: payload.cidnotify_packet,
        storage_blob: payload.storage_blob,
        invite_packet: payload.invite_packet,
        revocation_push: payload.revocation_push,
        deposited_at: ctx.mint_hlc().await,
        deposited_by: ctx.device_id(),
        ingested_by: BTreeSet::new(),
    };
```

(Confirm `MAX_DEPOSIT_INVITE_BYTES` exists in `butler_deposit.rs`; the invite path uses it at ~640. Reuse it as the frame-size bound for the revocation.)

- [ ] **Step 4: Run to verify pass**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(handle_deposit_core)'`
Expected: PASS (new revocation tests + existing message/invite acceptor tests).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/iroh_butler_acceptor.rs
git commit -m "ZEB-691: butler handle_deposit_core arm for device-revocation deposits"
```

---

### Task B5: Recipient inbox-sweeper revocation arm

**Files:**
- Modify: `src-tauri/src/dm_inbox_ingest.rs` (`DmInboxIngestCtx` trait at ~69; `ProdDmInboxIngestCtx` at ~813; `ingest_pending` dispatch at ~169; any test probe impl of the trait)
- Test: same file (`#[cfg(test)]`)

**Interfaces:**
- Produces: `DmInboxIngestCtx::apply_revocation(&self, entry: &DmInboxEntry) -> Result<bool, String>`; `ProdDmInboxIngestCtx.notify_owner_state_dirty: Option<Arc<dyn Fn() + Send + Sync>>`.
- Consumes: `handle_revocation_push` (existing), `DmInboxEntry.revocation_push` (B1), `entry.sender_owner`, the ctx's `crdt_state` + `revoked`.

- [ ] **Step 1: Write the failing test**

Add a test that builds a `DmInboxEntry` with a real master-signed `revocation_push` (reuse the revocation test helper), a `ProdDmInboxIngestCtx` (or the test probe) whose `crdt_state` has the friend Active, a counting `notify_owner_state_dirty`, then calls `apply_revocation` and asserts: `Ok(true)` on first apply, the CRDT `revoked_dm_devices` gained the key, the dirty counter incremented once; a second apply returns `Ok(false)` and does NOT increment the counter (idempotent). Name it `apply_revocation_applies_and_marks_dirty_once`. Also add a `ingest_pending` sweeper test that a `revocation_push` entry gets `ingested_by`-marked and `changed == true`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(apply_revocation_applies_and_marks_dirty_once)'`
Expected: FAIL — `apply_revocation` / the ctx field don't exist.

- [ ] **Step 3: Add the trait method, field, prod impl, and dispatch arm**

Trait (`DmInboxIngestCtx`, ~69):

```rust
    /// ZEB-691: apply a deposited device-revocation entry. Re-verifies the certs
    /// (never trust the butler), applies via `handle_revocation_push`, and marks
    /// owner-state dirty on a genuine insert. Returns `Ok(inserted)`.
    async fn apply_revocation(&self, entry: &DmInboxEntry) -> Result<bool, String>;
```

Field (`ProdDmInboxIngestCtx`, ~837 after `revoked`):

```rust
    /// ZEB-691: owner-state SyncEngine dirty hook. A deposited revocation entry
    /// is eventually GC'd, so unlike CidNotify/invite (which lean on
    /// re-delivery), the recover MUST persist the owner-state mutation itself.
    /// `None` only in unit tests that assert without persistence.
    pub notify_owner_state_dirty: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
```

Prod impl (in `impl DmInboxIngestCtx for ProdDmInboxIngestCtx`):

```rust
    async fn apply_revocation(&self, entry: &DmInboxEntry) -> Result<bool, String> {
        let rp = entry
            .revocation_push
            .as_deref()
            .ok_or("apply_revocation: entry has no revocation_push")?;
        let packet = crate::dm_envelope::decode_packet(rp)
            .map_err(|e| format!("decode revocation_push: {e}"))?;
        let crate::dm_envelope::DmPacket::RevocationPush { revocation, enrollment } = packet else {
            return Err("revocation_push is not a RevocationPush packet".into());
        };
        let inserted = {
            let mut state = self.crdt_state.lock().await;
            crate::dm_outbox::handle_revocation_push(
                &mut state,
                crate::owner_state_types::OwnerAddr(entry.sender_owner),
                &revocation,
                &enrollment,
                &self.revoked,
            )
            .map_err(|e| format!("handle_revocation_push: {e:?}"))?
        };
        if inserted {
            if let Some(mark) = &self.notify_owner_state_dirty {
                mark();
            }
        }
        Ok(inserted)
    }
```

Dispatch arm in `ingest_pending`, INSERTED BEFORE the `if entry.cidnotify_packet.is_none()` invite-only branch (~169), AFTER the `ingested_by` skip (~163):

```rust
        // ZEB-691: a device-revocation deposit (no cidnotify, no invite). Apply
        // it before the invite-only branch would otherwise swallow the
        // `cidnotify_packet == None` case.
        if entry.revocation_push.is_some() {
            match ctx.apply_revocation(entry).await {
                Ok(_) => {
                    entry.ingested_by.insert(self_id.clone());
                    changed = true;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "ZEB-691: revocation recover failed; leaving entry pending");
                }
            }
            continue;
        }
```

Add `apply_revocation` to any `#[cfg(test)]` probe impl of `DmInboxIngestCtx` (grep `impl DmInboxIngestCtx for`) — a probe can decode + call `handle_revocation_push` against a test `crdt_state`/`revoked`, or record the call.

- [ ] **Step 4: Run to verify pass**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(apply_revocation) | test(ingest_pending)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/dm_inbox_ingest.rs
git commit -m "ZEB-691: recipient inbox-sweeper arm applies deposited revocations + notify_dirty"
```

---

### Task B6: Boot wiring — `NodeState.butler_deposit_client` + ingest-ctx dirty hook

**Files:**
- Modify: `src-tauri/src/lib.rs` (`NodeState` struct at ~895; the deposit-client construction at ~9783/9800; `ProdDmInboxIngestCtx` construction at ~5167-5183; `owner_state_engine_for_dirty` at ~4856)

**Interfaces:**
- Produces: `NodeState.butler_deposit_client: Option<Arc<dyn ButlerDepositClient>>`; the ingest ctx wired with `notify_owner_state_dirty: Some(...)`.

This task is boot wiring; it is verified by compile + the existing DM integration tests (and Task B7's end-to-end test). No new unit test.

- [ ] **Step 1: Add the NodeState field**

Add to `NodeState` (near `dm_outbox` at ~895):

```rust
    /// ZEB-691: the butler deposit client (same `Arc` set on the `DmOutbox`),
    /// exposed here so `push_revocation_to_friends` can deposit a revocation to a
    /// friend's own butler set. `None` until the node binds iroh.
    butler_deposit_client: Option<std::sync::Arc<dyn crate::butler_deposit::ButlerDepositClient>>,
```

Initialize it to `None` in every `NodeState { … }` constructor (grep `NodeState {` — e.g. ~1785).

- [ ] **Step 2: Store the client at construction**

At the deposit-client construction (~9783-9800), the code builds `deposit_client` and calls `.set_butler_deposit_client(deposit_client)`. Capture a clone into NodeState. Right after `set_butler_deposit_client`, add (adapt to the exact lock/handle in scope — the NodeState mutex is the one `crdt_state`/`dm_outbox` are stored through):

```rust
                                        // ZEB-691: also expose the butler deposit
                                        // client on NodeState for the revocation
                                        // fan-out (push_revocation_to_friends).
                                        if let Ok(mut g) = state.lock() {
                                            g.butler_deposit_client = Some(deposit_client.clone());
                                        }
```

(`deposit_client` must be cloned BEFORE the `set_butler_deposit_client(deposit_client)` move, or reorder so the clone happens first. Match the variable/lock names present at that site.)

- [ ] **Step 3: Thread the dirty hook into the ingest ctx**

At the `ProdDmInboxIngestCtx { … }` construction (~5167-5183), add the field, building the closure from `owner_state_engine_for_dirty` (in scope from ~4856, as used by the tunnel drain at ~9672-9676):

```rust
                            notify_owner_state_dirty: {
                                let e = std::sync::Arc::clone(&owner_state_engine_for_dirty);
                                Some(std::sync::Arc::new(move || e.notify_dirty())
                                    as std::sync::Arc<dyn Fn() + Send + Sync>)
                            },
```

- [ ] **Step 4: Verify compile + affected tests**

Run: `cargo check --locked --all-targets --features test-fixtures`
Then: `cargo nextest run --locked --features test-fixtures -E 'test(dm_revocation) | test(dm_inbox)'`
Expected: compile clean; existing DM revocation/inbox tests pass.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "ZEB-691: wire NodeState butler deposit client + ingest-ctx owner-state dirty hook"
```

---

### Task B7: Send side — deposit revocations to friends' butlers + end-to-end test

**Files:**
- Modify: `src-tauri/src/owner_commands.rs` (`push_revocation_to_friends` at ~1188; its call sites / snapshot at ~918-922 and the `AlreadyRevoked` retry arm at ~1090)
- Test: `src-tauri/tests/dm/dm_revocation_cutoff_integration.rs` (extend the #471 harness) OR a `#[cfg(test)]` unit in `owner_commands.rs` with a mock `ButlerDepositClient`

**Interfaces:**
- Consumes: `NodeState.butler_deposit_client` (B6), `ButlerDepositRequest.revocation_push` (B2), the existing `wire` bytes built in `push_revocation_to_friends`.

- [ ] **Step 1: Write the failing test**

Preferred: a unit test with a mock `ButlerDepositClient` (mirror `dm_outbox.rs`'s `MockDepositClient`) that records each `ButlerDepositRequest`. Drive `push_revocation_to_friends` (add the client as a parameter — see Step 3) with two Active friends + a real revocation, and assert: one deposit per Active friend, each with `revocation_push == Some(wire)`, `cidnotify_packet == None`, `invite_packet == None`, `message_cid == None`. Name it `push_revocation_to_friends_deposits_to_each_active_friend_butler`.

Optionally add an end-to-end integration test in `tests/dm/dm_revocation_cutoff_integration.rs` (a butler-deposit variant of `dm_only_contact_cutoff_via_revocation_push`): deposit the revocation into a `DmInboxDoc`, run `ingest_pending` on the recipient ctx, assert the cutoff projection now rejects the revoked device.

- [ ] **Step 2: Run to verify failure**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(push_revocation_to_friends_deposits_to_each_active_friend_butler)'`
Expected: FAIL — `push_revocation_to_friends` does not deposit.

- [ ] **Step 3: Add the butler deposit to the fan-out**

Change `push_revocation_to_friends`'s signature to take the client, and deposit per friend alongside the tunnel send:

```rust
async fn push_revocation_to_friends(
    crdt_state: &std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    mgr: &std::sync::Arc<crate::tunnel_manager::TunnelManager>,
    butler: Option<&std::sync::Arc<dyn crate::butler_deposit::ButlerDepositClient>>,
    trust_snapshot: &harmony_owner::state::OwnerState,
    revocation: &RevocationCert,
) {
    // … unchanged: build `enrollment`, `packet`, `wire`, `targets` …
    for owner in targets {
        crate::iroh_tunnel_dm_transport::send_packet_to_owner_tunnels(
            crdt_state, mgr, owner, &wire,
        )
        .await;
        // ZEB-691: also deposit to the friend's own butler set (their always-on
        // fleet) so an offline DM-only friend recovers the revocation on
        // reconnect. Best-effort: no butler / no fresh set simply skips.
        if let Some(butler) = butler {
            let req = crate::butler_deposit::ButlerDepositRequest {
                entry_id: crate::owner_state_types::OutboxEntryId([0u8; 16]),
                recipient_owner: owner,
                space_id: crate::owner_state_types::SpaceId([0u8; 16]),
                message_cid: None,
                cidnotify_packet: None,
                invite_packet: None,
                revocation_push: Some(wire.clone()),
                now_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
            };
            let _ = butler.deposit(&req).await;
        }
    }
}
```

Update the snapshot at ~918-922 to also grab the client, and pass it at both call sites (the main hook ~983 and the `AlreadyRevoked { is_self: false }` retry arm ~1090):

```rust
    let (crdt_state_for_push, tunnel_manager_for_push, butler_client_for_push) = {
        let g = state.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        (g.crdt_state.clone(), g.tunnel_manager.clone(), g.butler_deposit_client.clone())
    };
```

At each `push_revocation_to_friends(crdt, mgr, &trust_snapshot, &cert_for_feed).await` call, insert `butler_client_for_push.as_ref()` as the new third argument.

- [ ] **Step 4: Run to verify pass + full gates**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(push_revocation_to_friends) | test(dm_revocation)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/owner_commands.rs src-tauri/tests/dm/dm_revocation_cutoff_integration.rs
git commit -m "ZEB-691: deposit device revocations to friends' butlers (send side + retry arm)"
```

---

## Final gates (run before opening the PR)

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --all-targets --features test-fixtures
cargo check --locked --all-targets --features test-fixtures   # MSRV parity
```

All green + both ZEB-691 (butler-rung delivery + recover) and ZEB-692 (cap + GC) covered by tests → open the bundled PR (`Closes ZEB-691`, `Closes ZEB-692`), fire `@coderabbitai review` once, and converge the bot buckets per the standing loop.

---

## Self-review notes

- **Spec coverage:** Part A covers ZEB-692 (cap A1/A2, GC A3, both convergent). Part B covers ZEB-691 (wire B1/B2, verify-core B3, butler arm B4, recover B5, boot wiring B6, send B7). Every §Seams entry in the design maps to a task.
- **Type consistency:** `revocation_push: Option<Vec<u8>>` on `DepositPayload` (B2), `ButlerDepositRequest` (B2), `DmInboxEntry` (B1); `REVOCATION_DEPOSIT_MARKER` defined B2, used B2 (client) + B4 (acceptor); `verify_revocation_push` defined B3, used B3 (handler) + B4 (butler); `DmInboxDoc::revoke_key` defined B1, used B4; `notify_owner_state_dirty` field defined B5, wired B6.
- **Ordering:** B1/B2 (wire) → B3 (core) → B4 (acceptor, uses B1/B2/B3) → B5 (recover, uses B1) → B6 (wiring, uses B5 field + B2 client) → B7 (send, uses B2/B6). Part A is independent of Part B and lands first.
