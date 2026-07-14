# ZEB-580 S2 — Shared-community DM revocation cutoff Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A DM signed by a device #2 the sender's owner has revoked in a community we share stops being accepted — dropped at verify time, not delivered, not acked.

**Architecture:** A new synchronous `RevokedDeviceProjection` (owner → set of revoked #2 Ed25519 keys) is a pure derivation of the community materialized view. It's fed from the same two `start_node_inner` choke points that already feed `MembershipProjection`, and read on the DM receive path: after a DM's signature verifies, the signer's `combined_pub[32..64]` (its Ed25519 half) is membership-tested against the sender-owner's revoked set; a hit drops the DM. Legacy #3 signers are unaffected for free (a #3 Ed25519 is never an enrolled device key, so it can never appear in any community's `revoked_device_keys`).

**Tech Stack:** Rust (harmony-app crate under `src-tauri/`), Tauri, Svelte/Vitest frontend. `std::sync::RwLock` for the projection (sync read on the receive path). `harmony_owner` certs.

## Global Constraints

Copied from the design spec (`docs/specs/2026-07-13-zeb-580-dm-signing-migration-design.md` §5, §8) and standing repo rules. Every task's requirements implicitly include this section.

- **No `FILE_VERSION` / packet-version / wire-format bumps.** S2 adds no wire fields. `revoked_device_keys` already ships on the community wire (`rename "rk"`, additive, from ZEB-668). The projection is in-memory only.
- **Revoked key identity is the raw 32-byte Ed25519 verify key** (`[u8; 32]`) = `EnrollmentCert.device_pubkeys.classical.ed25519_verify` = a #2 signer's `device2_combined_pub(cert)[32..64]`. NOT a device hash. The projection value type is `BTreeSet<[u8; 32]>`.
- **`by_owner` key is the *resolved* sender owner** (`resolve_signed_origin_owner` result for CidNotify/Ack; `signed.inviter` for Invite), which is cryptographically bound to the authenticated peer before the cutoff runs. Never key on a payload-controlled, unverified owner field.
- **The cutoff is uniform and unconditional** — extract `combined_pub[32..64]`, membership-test the revoked set, drop on hit. Do NOT add a `#2`-vs-`#3` discriminator; a legacy `#3` Ed25519 is never enrolled, so it can never be in `revoked_device_keys`, making the check a strict no-op for `#3` (spec §5.2 "legacy #3 DMs are not subject to the cutoff" — realized without branching).
- **Sticky / monotonic revocation** (spec §5.1, §8.3): the projection only ever UNIONs; a key present in any joined community's revoked set stays revoked for the session. Leaving a community does NOT retract (losing the fact is the safe direction). No un-revoke path.
- **`verify_dm_packet_signature` stays unchanged** (spec §6). The cutoff is a distinct step in the three receive verify helpers, AFTER signature verification succeeds, BEFORE any state mutation / CAS fetch / ack build.
- **No downgrade hole** (spec §8.2): a `#3`-shaped packet is accepted only against a `#3` pub the receiver already trusted and cached; the cutoff applies to `#2` only and `#3` was never subject to it, so the incentive runs the safe direction. This must be an explicit test, not just prose.
- **Projection read is synchronous** (`std::sync::RwLock`, poisoned-lock recovered via `.unwrap_or_else(|e| e.into_inner())`), never held across an `.await` — it is consulted inside the async owner-state critical section, so it must not introduce an async lock-ordering hazard.
- **Q1 (settled): the projection lives in a NEW standalone leaf module** `src-tauri/src/revoked_device_projection.rs` that imports only `crate::owner_state_types`. NOT `network_health` (would create the only `dm_* → network_health` import edge — a layering inversion). `lib.rs` bridges materialize→projection; `dm_outbox` reads via a handle.
- **Gates (CI-parity):** `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`; `npx tsc --noEmit`; `npx vitest run`. Cargo from `src-tauri/`, npm from repo root. Iterative gates via `scripts/test-select --context task`; full `--workspace --all-targets` sweep ONLY at the final task (a lib change relinks ~97 integ binaries ~50min).

---

## File Structure

- **Create:** `src-tauri/src/revoked_device_projection.rs` — the `RevokedDeviceProjection` type (leaf; depends only on `owner_state_types`). Owns the projection struct, its feed method (`union_from_members`), its read method (`is_revoked`), and unit tests.
- **Modify:** `src-tauri/src/lib.rs` — register the module (`pub mod revoked_device_projection;`); construct the projection at boot (~`:4271`); clone it into the on-epoch hook (~`:6627`/`:6767`) and feed it at both choke points (live ~`:7112`, boot ~`:7832`); clone it into `ProdDmInboxIngestCtx` (~`:5087`) and thread it into the tunnel drain call to `ingest_dm_packet` (~`:9472`).
- **Modify:** `src-tauri/src/dm_outbox.rs` — add `DmReceiveError::SignerDeviceRevoked`; add the cutoff to the three verify helpers (`verify_cidnotify_sender_binding` `:3298`, `apply_invite` `:2350`, `handle_ack` `:2187`), each gaining a `revoked: &RevokedDeviceProjection` parameter.
- **Modify:** `src-tauri/src/dm_inbox_ingest.rs` — thread the handle through `ingest_dm_packet` (`:428`, new param) and `ProdDmInboxIngestCtx` (`:757`, new field + its `verify`/`apply_invite_only` pass-through).
- **Modify:** `src-tauri/src/tunnel_task.rs` — pass the handle at the `ingest_dm_packet` driver call (`:1654`).
- **Modify:** `src/lib/components/RemoveDeviceDialog.svelte` (`:99-100`) + `src/lib/components/__tests__/RemoveDeviceDialog.test.ts` (`:53`, `:113`) — narrow the DM honesty caveat.
- **Modify:** `docs/specs/2026-07-11-zeb-668-device-management-design.md` (§8 row `:297`, §9 `:313`) — update the honesty ledger.
- **Create:** `src-tauri/tests/dm/dm_revocation_cutoff_integration.rs` — the e2e revoked-DM-dropped gate.

---

## Task 1: `RevokedDeviceProjection` type + module

**Files:**
- Create: `src-tauri/src/revoked_device_projection.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod revoked_device_projection;` next to the other `pub mod` declarations, e.g. after `pub mod network_health;` at `lib.rs:249`)

**Interfaces:**
- Consumes: `crate::owner_state_types::OwnerAddr` (16-byte newtype).
- Produces:
  - `RevokedDeviceProjection` (`#[derive(Clone, Default)]`) — cheap handle clone; shared `Arc<RwLock<..>>` inner.
  - `pub fn new() -> Self`
  - `pub fn union_from_members<'a, I>(&self, members: I) where I: IntoIterator<Item = (OwnerAddr, &'a std::collections::BTreeSet<[u8; 32]>)>` — sticky union; never removes.
  - `pub fn is_revoked(&self, owner: &OwnerAddr, ed25519: &[u8; 32]) -> bool` — synchronous membership test.

- [ ] **Step 1: Write the failing tests**

Create `src-tauri/src/revoked_device_projection.rs` with the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::OwnerAddr;
    use std::collections::BTreeSet;

    fn set(keys: &[[u8; 32]]) -> BTreeSet<[u8; 32]> {
        keys.iter().copied().collect()
    }

    #[test]
    fn union_and_is_revoked_roundtrip() {
        let p = RevokedDeviceProjection::new();
        let owner = OwnerAddr([0x11; 16]);
        let k = [0xaa; 32];
        assert!(!p.is_revoked(&owner, &k), "empty projection revokes nothing");
        let s = set(&[k]);
        p.union_from_members(std::iter::once((owner, &s)));
        assert!(p.is_revoked(&owner, &k));
        assert!(!p.is_revoked(&owner, &[0xbb; 32]), "unrelated key not revoked");
        assert!(!p.is_revoked(&OwnerAddr([0x22; 16]), &k), "revocation is per-owner");
    }

    #[test]
    fn union_is_sticky_across_a_simulated_community_leave() {
        // A later materialize that omits the owner entirely (node left the
        // community that carried the revocation) must NOT un-revoke.
        let p = RevokedDeviceProjection::new();
        let owner = OwnerAddr([0x11; 16]);
        let k = [0xaa; 32];
        let s = set(&[k]);
        p.union_from_members(std::iter::once((owner, &s)));
        assert!(p.is_revoked(&owner, &k));
        // Next feed round carries no members at all (left every shared community).
        p.union_from_members(std::iter::empty());
        assert!(p.is_revoked(&owner, &k), "sticky: leave does not retract");
        // Next feed round carries the owner with an EMPTY revoked set.
        let empty = BTreeSet::new();
        p.union_from_members(std::iter::once((owner, &empty)));
        assert!(p.is_revoked(&owner, &k), "sticky: empty set does not retract");
    }

    #[test]
    fn union_accumulates_across_owners_and_communities() {
        let p = RevokedDeviceProjection::new();
        let (o1, o2) = (OwnerAddr([1; 16]), OwnerAddr([2; 16]));
        let (k1, k2) = ([0x01; 32], [0x02; 32]);
        p.union_from_members(std::iter::once((o1, &set(&[k1]))));
        p.union_from_members(std::iter::once((o2, &set(&[k2]))));
        // A second community adds another key for o1.
        p.union_from_members(std::iter::once((o1, &set(&[[0x03; 32]]))));
        assert!(p.is_revoked(&o1, &k1));
        assert!(p.is_revoked(&o1, &[0x03; 32]));
        assert!(p.is_revoked(&o2, &k2));
    }

    #[test]
    fn clone_shares_state() {
        let p = RevokedDeviceProjection::new();
        let handle = p.clone();
        let owner = OwnerAddr([0x11; 16]);
        let k = [0xaa; 32];
        p.union_from_members(std::iter::once((owner, &set(&[k]))));
        assert!(handle.is_revoked(&owner, &k), "clone sees writes via shared Arc");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail (won't compile — type absent)**

Run: `cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(revoked_device_projection)'`
Expected: compile error (`RevokedDeviceProjection` not found).

- [ ] **Step 3: Write the implementation** (prepend above the test module)

```rust
//! ZEB-580 S2: a synchronous projection answering "is owner X's device D
//! revoked?" for the DM receive-path cutoff. A pure derivation of the community
//! materialized view (`MemberState.revoked_device_keys`), aggregated by owner
//! across every community this node is joined in. Sticky/monotonic within a
//! session (spec §5.1): a key present in any joined community's revoked set
//! stays revoked; leaving a community does not retract. Sibling in spirit to
//! `network_health::MembershipProjection`, but a standalone leaf (depends only
//! on `owner_state_types`) because its consumer is the DM receive path, not the
//! network-health panel — co-locating in `network_health` would force the only
//! `dm_* -> network_health` import edge (a layering inversion).

use crate::owner_state_types::OwnerAddr;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

#[derive(Clone, Default)]
pub struct RevokedDeviceProjection {
    // owner -> revoked #2 ed25519 verify keys. std RwLock (NOT tokio): the read
    // sits on the DM receive path inside the owner-state critical section and
    // must be synchronous / never held across an .await.
    by_owner: Arc<RwLock<BTreeMap<OwnerAddr, BTreeSet<[u8; 32]>>>>,
}

impl RevokedDeviceProjection {
    pub fn new() -> Self {
        Self::default()
    }

    /// Union each `(owner, revoked_keys)` into the projection. Sticky: existing
    /// keys are never removed, so an owner absent from `members` (node left the
    /// carrying community) or present with an empty set retains prior tombstones.
    pub fn union_from_members<'a, I>(&self, members: I)
    where
        I: IntoIterator<Item = (OwnerAddr, &'a BTreeSet<[u8; 32]>)>,
    {
        let mut guard = self.by_owner.write().unwrap_or_else(|e| e.into_inner());
        for (owner, keys) in members {
            if keys.is_empty() {
                continue;
            }
            guard.entry(owner).or_default().extend(keys.iter().copied());
        }
    }

    /// True iff `ed25519` is a revoked #2 key for `owner`. Synchronous.
    pub fn is_revoked(&self, owner: &OwnerAddr, ed25519: &[u8; 32]) -> bool {
        let guard = self.by_owner.read().unwrap_or_else(|e| e.into_inner());
        guard.get(owner).is_some_and(|s| s.contains(ed25519))
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(revoked_device_projection)'`
Expected: 4 passed.

- [ ] **Step 5: Gate + commit**

Run: `cd src-tauri && cargo fmt --all && cargo clippy --locked --lib --features test-fixtures --no-deps -- -D warnings`
Then:
```bash
git add src-tauri/src/revoked_device_projection.rs src-tauri/src/lib.rs
git commit -m "ZEB-580 S2 (T1): RevokedDeviceProjection type + module"
```

---

## Task 2: Feed the projection from the community materialize choke points

**Files:**
- Modify: `src-tauri/src/lib.rs` — construct (~`:4271`), clone into the on-epoch hook (~`:6627`, per-invocation ~`:6767`), feed at the live delta hook (~`:7112`) and the boot-replay seed loop (~`:7832`).

**Interfaces:**
- Consumes: `RevokedDeviceProjection` (Task 1); `mat.members: &BTreeMap<OwnerAddr, MemberState>` (already materialized in-scope at both choke points); `MemberState.revoked_device_keys: BTreeSet<[u8; 32]>` (`community_membership.rs:1679`).
- Produces: a boot-constructed `revoked_device_projection` handle that Tasks 3–5 consume, populated live + on boot.

**Context:** `MembershipProjection` is the exact template. It is constructed at `lib.rs:4271`, cloned into the hook at `:6627` (and `:6767` per async invocation), driven live at `:7112` (`membership_projection.set_community_members(community_id, joined)`), and seeded on boot at `:7832`. Both choke points already compute `mat`/`current` = `st.materialized(engine.admin_addr())` under the engine lock. The projection feed piggybacks the SAME `mat.members` — no new hook into `community_membership.rs`.

- [ ] **Step 1: Write the failing test** (a focused wiring unit in `lib.rs`'s test module that proves the feed helper closure shape; the full boot wiring is proven by Task 8's integration test)

Because the choke points live inside `start_node_inner` (not unit-testable without booting a node), Task 1 already tests the projection's `union_from_members` contract exhaustively. This task's correctness is: (a) `union_from_members` is called at both choke points over `mat.members`, and (b) it compiles. Add ONE assertion-bearing unit test in `lib.rs` `#[cfg(test)]` that exercises the exact feed expression against a hand-built members map, guarding the `(OwnerAddr, &BTreeSet<[u8;32]>)` projection of `MemberState`:

```rust
#[test]
fn revoked_projection_feed_expression_reads_member_revoked_keys() {
    use crate::owner_state_types::OwnerAddr;
    use std::collections::{BTreeMap, BTreeSet};
    // Build a minimal members map mirroring MaterializedMembership.members.
    let owner = OwnerAddr([0x33; 16]);
    let mut member = crate::community_membership::MemberState::default();
    member.revoked_device_keys = BTreeSet::from([[0xcd; 32]]);
    let members: BTreeMap<OwnerAddr, crate::community_membership::MemberState> =
        BTreeMap::from([(owner, member)]);

    let proj = crate::revoked_device_projection::RevokedDeviceProjection::new();
    // The EXACT feed expression used at both choke points:
    proj.union_from_members(members.iter().map(|(o, m)| (*o, &m.revoked_device_keys)));

    assert!(proj.is_revoked(&owner, &[0xcd; 32]));
}
```

> If `MemberState::default()` is not available/public, construct it via the same test helper the `community_membership` tests use (check `community_membership.rs` test module) or make the test build a `MemberState` literal with `..Default::default()`; the assertion is the point.

- [ ] **Step 2: Run it to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(revoked_projection_feed_expression)'`
Expected: FAIL (or compile error) until the module is wired — Task 1's module exists, so this should PASS immediately if `MemberState`'s field is accessible; if so, keep it as a guard and proceed to the wiring steps below (its value is pinning the feed expression against `MemberState`'s shape).

- [ ] **Step 3: Construct the projection at boot**

At `lib.rs:4271`, immediately after `let membership_projection = crate::network_health::MembershipProjection::new();`, add:

```rust
    // ZEB-580 S2: revoked-device projection, fed from the same community
    // materialize choke points as membership_projection below; read by the DM
    // receive cutoff. Sticky union across joined communities.
    let revoked_device_projection =
        crate::revoked_device_projection::RevokedDeviceProjection::new();
```

- [ ] **Step 4: Clone into the on-epoch hook**

At `lib.rs:6627` (next to `let membership_projection_for_hook = membership_projection.clone();`) add:
```rust
    let revoked_device_projection_for_hook = revoked_device_projection.clone();
```
And at the per-invocation clone site (`:6767`, where `membership_projection` is moved into the async block), add the matching per-invocation clone of `revoked_device_projection_for_hook`.

- [ ] **Step 5: Feed at the live delta choke point (`~lib.rs:7092-7116`)**

In the block that computes `let mat = st.materialized(engine.admin_addr());` and calls `membership_projection.set_community_members(community_id, joined)`, after that call (using the SAME already-materialized `mat`, before the guard/lock is dropped is fine since the read is over the owned map), add:

```rust
        // ZEB-580 S2: union this community's revoked #2 keys into the sticky
        // by-owner projection (same materialized view; no retract on leave).
        revoked_device_projection
            .union_from_members(mat.members.iter().map(|(o, m)| (*o, &m.revoked_device_keys)));
```

> Note: unlike `membership_projection` (which `remove_community` on leave), the revoked projection is sticky — it is fed on EVERY delta and never retracts, so there is no `else`/remove branch. Feed it unconditionally whenever `mat` is in hand (even the `remove_community` branch should still union first — a revocation learned right before a leave must survive).

- [ ] **Step 6: Feed at the boot-replay seed loop (`~lib.rs:7787-7828`)**

In the boot loop that computes `current = st.materialized(engine.admin_addr())`, add the same union over `current.members` (unconditional — before the `is_joined` gate that only affects the membership seed):

```rust
        // ZEB-580 S2: seed the revoked projection on restart from persisted
        // materialized state (the on-epoch hook only fires on NEW deltas).
        revoked_device_projection
            .union_from_members(current.members.iter().map(|(o, m)| (*o, &m.revoked_device_keys)));
```

- [ ] **Step 7: Gate + commit** (the handle is now populated; Tasks 3–5 consume it — it is currently unused, which clippy will flag; suppress narrowly or proceed since Task 3 consumes it. To keep this task self-clean, add `let _ = &revoked_device_projection;` only if clippy's `unused` fires, and remove it in Task 3.)

Run: `cd src-tauri && cargo fmt --all && scripts/test-select --context task` (paste the `round=… bucket=…` line into the task report). Then:
```bash
git add src-tauri/src/lib.rs
git commit -m "ZEB-580 S2 (T2): feed RevokedDeviceProjection from materialize choke points"
```

---

## Task 3: CidNotify cutoff — `verify_cidnotify_sender_binding` + error variant

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` — add `DmReceiveError::SignerDeviceRevoked` (`:3043` enum); add `revoked: &crate::revoked_device_projection::RevokedDeviceProjection` param to `verify_cidnotify_sender_binding` (`:3298`) and its wrapper `verify_cidnotify_admission` (`:3271`); add the cutoff.
- Modify: `src-tauri/src/dm_inbox_ingest.rs` — thread the handle into `ingest_dm_packet` (`:428`, new `revoked: &RevokedDeviceProjection` param) for the CidNotify dispatch, and into `ProdDmInboxIngestCtx` (`:757`, new field + `verify()` `:804`).
- Modify: `src-tauri/src/lib.rs` — pass the handle at the tunnel drain call to `ingest_dm_packet` (`~:9472`) and into the `ProdDmInboxIngestCtx` construction (`~:5087`).
- Modify: `src-tauri/src/tunnel_task.rs` — pass the handle at the `ingest_dm_packet` driver (`:1654`).

**Interfaces:**
- Consumes: `RevokedDeviceProjection` handle (Task 2); `verify_cidnotify_sender_binding` returns `(resolved_owner: OwnerAddr, identity_pub: [u8; 64])`.
- Produces: CidNotify DMs from a revoked #2 device are dropped with `DmReceiveError::SignerDeviceRevoked`.

- [ ] **Step 1: Write the failing tests** (in `dm_outbox.rs` `#[cfg(test)]`)

```rust
#[test]
fn cidnotify_from_revoked_device2_is_cut_off() {
    // Build state with a cached #2 signer whose ed25519 the projection revokes.
    // (Mirror the existing verify_cidnotify_sender_binding happy-path test setup;
    //  find it via `test(verify_cidnotify)` and clone its fixture.)
    let (state, signed, signature, signed_bytes, owner, combined_pub) =
        cidnotify_verify_fixture(); // existing helper or inline the happy-path setup
    let ed25519: [u8; 32] = combined_pub[32..64].try_into().unwrap();

    // Empty projection -> admitted.
    let clean = crate::revoked_device_projection::RevokedDeviceProjection::new();
    assert!(verify_cidnotify_sender_binding(&state, &signed, &signature, &signed_bytes, &clean).is_ok());

    // Revoked projection -> SignerDeviceRevoked.
    let revoked = crate::revoked_device_projection::RevokedDeviceProjection::new();
    revoked.union_from_members(std::iter::once((owner, &std::collections::BTreeSet::from([ed25519]))));
    let err = verify_cidnotify_sender_binding(&state, &signed, &signature, &signed_bytes, &revoked)
        .expect_err("revoked signer must be cut off");
    assert_eq!(err, DmReceiveError::SignerDeviceRevoked);
}

#[test]
fn cidnotify_cutoff_is_noop_for_legacy_device3_signer() {
    // A #3 signer's cached combined pub — its ed25519 half is a #3 identity key,
    // which is never an enrolled #2 key and so can never be in revoked_device_keys.
    // Even with a non-empty projection for the owner, a #3 packet is admitted.
    let (state, signed, signature, signed_bytes, owner, _combined3) =
        cidnotify_verify_fixture_device3(); // #3-cached variant
    let revoked = crate::revoked_device_projection::RevokedDeviceProjection::new();
    // Revoke some OTHER key for the same owner (a #2 key that isn't this #3 signer).
    revoked.union_from_members(std::iter::once((owner, &std::collections::BTreeSet::from([[0x99; 32]]))));
    assert!(
        verify_cidnotify_sender_binding(&state, &signed, &signature, &signed_bytes, &revoked).is_ok(),
        "legacy #3 signer is not subject to the cutoff (no downgrade hole)"
    );
}
```

> Reuse the existing `verify_cidnotify_sender_binding` happy-path test's fixture construction — locate it with `rg 'fn .*verify_cidnotify' src-tauri/src/dm_outbox.rs` and factor a small helper if needed. The two new tests differ only by the projection argument.

- [ ] **Step 2: Run to verify failure**

Run: `cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(cidnotify_from_revoked) + test(cidnotify_cutoff_is_noop)'`
Expected: compile error (extra arg / missing variant).

- [ ] **Step 3: Add the error variant**

In the `DmReceiveError` enum (`dm_outbox.rs:3043`), add:
```rust
    #[error("signer device is revoked")]
    SignerDeviceRevoked,
```

- [ ] **Step 4: Add the param + cutoff to `verify_cidnotify_sender_binding`**

Change the signature (`:3298`) to add a trailing param:
```rust
pub(crate) fn verify_cidnotify_sender_binding(
    state: &OwnerState,
    signed: &crate::dm_envelope::DmCidNotifySigned,
    signature: &[u8; 64],
    signed_bytes: &[u8],
    revoked: &crate::revoked_device_projection::RevokedDeviceProjection,
) -> Result<(OwnerAddr, [u8; 64]), DmReceiveError> {
```
Just before the final `Ok((resolved_owner, identity_pub))` (`:3318`), insert:
```rust
    // ZEB-580 S2: shared-community revocation cutoff. Drop if the signer's #2
    // ed25519 (combined_pub[32..64]) is revoked for the resolved owner. No-op
    // for legacy #3 signers (a #3 key is never an enrolled device key).
    let ed25519: [u8; 32] = identity_pub[32..64]
        .try_into()
        .expect("64 - 32 == 32");
    if revoked.is_revoked(&resolved_owner, &ed25519) {
        return Err(DmReceiveError::SignerDeviceRevoked);
    }
```
Add the same trailing `revoked` param to the wrapper `verify_cidnotify_admission` (`:3271`) and forward it to `verify_cidnotify_sender_binding`.

- [ ] **Step 5: Thread the handle through the callers**

1. `dm_inbox_ingest.rs::ingest_dm_packet` (`:428`) — add a `revoked: &crate::revoked_device_projection::RevokedDeviceProjection` param; pass it to the `verify_cidnotify_admission` call (`:585`).
2. `dm_inbox_ingest.rs::ProdDmInboxIngestCtx` (`:757`) — add field `pub revoked: std::sync::Arc<crate::revoked_device_projection::RevokedDeviceProjection>` (or a plain `RevokedDeviceProjection` since it is itself `Clone`+`Arc`-backed — prefer the bare `RevokedDeviceProjection` to match `MembershipProjection`'s by-value-handle style). In its `verify()` (`:804`), pass `&self.revoked` to `verify_cidnotify_sender_binding`.
3. `lib.rs` — construct `ProdDmInboxIngestCtx` (`~:5087`) with `revoked: revoked_device_projection.clone()`; pass `&revoked_device_projection` (or a clone) at the tunnel drain call to `ingest_dm_packet` (`~:9472`).
4. `tunnel_task.rs` (`:1654`) — pass a `&RevokedDeviceProjection` at its `ingest_dm_packet` call (its test/prod driver — thread a handle param through `tunnel_task`'s entry or default to `RevokedDeviceProjection::new()` for test drivers that don't exercise revocation).
5. `handle_cidnotify_lifted` (`:1983`, dormant) — add the param + pass through to keep the dormant path consistent (or pass a `&RevokedDeviceProjection::default()` if its callers can't supply one — prefer real threading).

> Existing tests that call `verify_cidnotify_sender_binding` / `ingest_dm_packet` with the old arity must be updated to pass `&RevokedDeviceProjection::new()` (empty ⇒ behavior unchanged). Grep for all call sites: `rg 'verify_cidnotify_sender_binding|verify_cidnotify_admission|ingest_dm_packet' src-tauri`.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(cidnotify)'`
Expected: the two new tests pass; existing CidNotify tests still pass.

- [ ] **Step 7: Gate + commit**

Run: `cd src-tauri && cargo fmt --all && scripts/test-select --context task`. Then:
```bash
git add -A
git commit -m "ZEB-580 S2 (T3): CidNotify revocation cutoff + SignerDeviceRevoked"
```

---

## Task 4: Invite cutoff — `apply_invite`

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` — add `revoked: &RevokedDeviceProjection` param to `apply_invite` (`:2350`); add the cutoff after signature verify (`:2447`), before the friend-tier fork (`:2454`).
- Modify: `src-tauri/src/dm_inbox_ingest.rs` — pass the handle at `apply_invite` call sites: the tunnel Invite dispatch inside `ingest_dm_packet`, and `ProdDmInboxIngestCtx::apply_invite_only` (`:956`).
- Modify: `src-tauri/src/lib.rs` / `tunnel_task.rs` — already threaded in Task 3; just forward.

**Interfaces:**
- Consumes: `RevokedDeviceProjection`; `apply_invite`'s `signed.inviter: OwnerAddr` (bound to the authenticated peer via `expected_inviter`) and `signer_identity_pub: [u8; 64]` (`:2408-2441`).
- Produces: a Space invite signed by a revoked #2 device is dropped (`SignerDeviceRevoked`), before any Space/cache write.

- [ ] **Step 1: Write the failing tests** (in `dm_outbox.rs` `#[cfg(test)]`, mirroring the existing `apply_invite_with_cert_caches_device2_identity` fixture)

```rust
#[test]
fn apply_invite_from_revoked_device2_is_cut_off() {
    // Reuse the S1 cert-carrying invite fixture (apply_invite_with_cert_caches_device2_identity).
    let (mut state, self_owner, device_id, signed, signature, signed_bytes, inviter, combined_pub) =
        invite_with_cert_fixture();
    let ed25519: [u8; 32] = combined_pub[32..64].try_into().unwrap();

    let clean = crate::revoked_device_projection::RevokedDeviceProjection::new();
    assert!(apply_invite(&mut state.clone(), self_owner, &device_id, signed.clone(), signature,
        &signed_bytes, 1_700_000_000_000, Some(inviter), true, &clean).is_ok());

    let revoked = crate::revoked_device_projection::RevokedDeviceProjection::new();
    revoked.union_from_members(std::iter::once((inviter, &std::collections::BTreeSet::from([ed25519]))));
    let err = apply_invite(&mut state, self_owner, &device_id, signed, signature,
        &signed_bytes, 1_700_000_000_000, Some(inviter), true, &revoked)
        .expect_err("invite from revoked device must be cut off");
    assert_eq!(err, DmReceiveError::SignerDeviceRevoked);
    // And nothing was written: no Space, no cache entry for the inviter's device.
    assert!(!state.spaces.contains_key(&signed_space_id_from_fixture()));
}

#[test]
fn apply_invite_legacy_no_cert_not_subject_to_cutoff() {
    // inviter_enrollment = None (#3): its inline pub's ed25519 is a #3 key, never
    // enrolled, so even a non-empty projection admits it (no downgrade hole).
    let (mut state, self_owner, device_id, signed, signature, signed_bytes, inviter, _pub3) =
        invite_legacy_no_cert_fixture();
    let revoked = crate::revoked_device_projection::RevokedDeviceProjection::new();
    revoked.union_from_members(std::iter::once((inviter, &std::collections::BTreeSet::from([[0x99; 32]]))));
    assert!(apply_invite(&mut state, self_owner, &device_id, signed, signature,
        &signed_bytes, 1_700_000_000_000, Some(inviter), true, &revoked).is_ok());
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(apply_invite_from_revoked) + test(apply_invite_legacy_no_cert_not_subject)'`
Expected: compile error (extra arg).

- [ ] **Step 3: Add the param + cutoff**

Add the trailing `revoked: &crate::revoked_device_projection::RevokedDeviceProjection` param to `apply_invite` (`:2350`). Immediately after `verify_dm_packet_signature(...)?;` (`:2447`), insert:
```rust
    // ZEB-580 S2: revocation cutoff — drop a Space invite signed by a revoked #2
    // device before any cache/Space write. `signed.inviter` is bound to the
    // authenticated peer above (expected_inviter). No-op for legacy #3 (its
    // inline pub's ed25519 is never an enrolled key).
    let inviter_ed25519: [u8; 32] = signer_identity_pub[32..64]
        .try_into()
        .expect("64 - 32 == 32");
    if revoked.is_revoked(&signed.inviter, &inviter_ed25519) {
        return Err(DmReceiveError::SignerDeviceRevoked);
    }
```

- [ ] **Step 4: Thread the handle through `apply_invite` callers**

- `ingest_dm_packet` Invite dispatch → pass the `revoked` param (already added to `ingest_dm_packet` in Task 3).
- `ProdDmInboxIngestCtx::apply_invite_only` (`:956`) → pass `&self.revoked`.
- Dormant `DmOutbox::handle_invite` (if it calls `apply_invite`) → pass through.
- Update all existing `apply_invite` test call sites to pass `&RevokedDeviceProjection::new()`. Grep: `rg 'apply_invite\(' src-tauri`.

- [ ] **Step 5: Run to verify pass**

Run: `cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(apply_invite)'`
Expected: new tests pass; existing `apply_invite*` tests pass.

- [ ] **Step 6: Gate + commit**

Run: `cd src-tauri && cargo fmt --all && scripts/test-select --context task`. Then:
```bash
git add -A
git commit -m "ZEB-580 S2 (T4): Invite revocation cutoff in apply_invite"
```

---

## Task 5: Ack cutoff — `handle_ack` (defense-in-depth, dormant path)

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` — add `revoked: &RevokedDeviceProjection` param to `handle_ack` (`:2187`); cutoff after verify (`:2204`), before `mark_ack_delivered` (`:2293`).
- Modify: the sole `handle_ack` caller (`handle_unicast`, dormant) — pass through.

**Interfaces:**
- Consumes: `RevokedDeviceProjection`; `handle_ack`'s `resolved_owner` (`:2207`) + `identity_pub` (`:2196`).

> `handle_ack` is dormant in production (Ack is rejected on the live tunnel at `dm_inbox_ingest.rs:556`). Include the cutoff for completeness so no future re-activation reintroduces a bypass.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn handle_ack_from_revoked_device2_is_cut_off() {
    let (mut outbox, mut state, signed, signature, signed_bytes, owner, combined_pub) =
        ack_verify_fixture(); // mirror the existing handle_ack happy-path test
    let ed25519: [u8; 32] = combined_pub[32..64].try_into().unwrap();
    let revoked = crate::revoked_device_projection::RevokedDeviceProjection::new();
    revoked.union_from_members(std::iter::once((owner, &std::collections::BTreeSet::from([ed25519]))));
    let err = outbox.handle_ack(&mut state, signed, signature, &signed_bytes, 1_700_000_000_000, &revoked)
        .await.expect_err("ack from revoked device dropped");
    assert_eq!(err, DmReceiveError::SignerDeviceRevoked);
}
```

- [ ] **Step 2: Run to verify failure** — Run: `cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(handle_ack_from_revoked)'` — Expected: compile error.

- [ ] **Step 3: Add param + cutoff** — trailing `revoked` param on `handle_ack`; after the verify block (`:2204`), before `mark_ack_delivered`:
```rust
    let ack_ed25519: [u8; 32] = identity_pub[32..64].try_into().expect("64 - 32 == 32");
    if revoked.is_revoked(&resolved_owner, &ack_ed25519) {
        return Err(DmReceiveError::SignerDeviceRevoked);
    }
```

- [ ] **Step 4: Thread through the dormant caller + fix existing `handle_ack` test call sites** (`&RevokedDeviceProjection::new()`). Grep: `rg 'handle_ack\(' src-tauri`.

- [ ] **Step 5: Run to verify pass** — Run: `cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(handle_ack)'` — Expected: pass.

- [ ] **Step 6: Gate + commit**
```bash
cd src-tauri && cargo fmt --all && scripts/test-select --context task
git add -A && git commit -m "ZEB-580 S2 (T5): Ack revocation cutoff (defense-in-depth)"
```

---

## Task 6: Honesty copy — narrow the DM caveat

**Files:**
- Modify: `src/lib/components/RemoveDeviceDialog.svelte` (`:99-100`)
- Modify: `src/lib/components/__tests__/RemoveDeviceDialog.test.ts` (`:53`, `:113`)
- Modify: `docs/specs/2026-07-11-zeb-668-device-management-design.md` (§8 row `:297`, §9 `:313`)

**Interfaces:** none (frontend copy + docs). Verified by `npx vitest run` + `npx tsc --noEmit`.

- [ ] **Step 1: Update the two failing test matchers first (TDD)**

The current DM matcher is `/aren't blocked yet/i` (lines 53 and 113). Change BOTH to assert the new narrowed copy. New matcher (matches the community-scoped cutoff sentence):
```ts
    expect(screen.getByText(/share a community with you stop being accepted/i)).toBeInTheDocument();
```
Apply at line 53 and line 113 (replace the `/aren't blocked yet/i` matcher). Keep the `/stop accepting new posts/i` assertion (52, 112) unchanged — feeds still stop.

- [ ] **Step 2: Run vitest to verify failure**

Run (repo root): `npx vitest run src/lib/components/__tests__/RemoveDeviceDialog.test.ts`
Expected: FAIL (old copy still says "aren't blocked yet").

- [ ] **Step 3: Narrow the Svelte copy** (`RemoveDeviceDialog.svelte:99-100`)

Replace the DM sentence:
```
Its direct messages are a separate surface and aren't blocked yet — that cutoff lands in follow-up work.
```
with:
```
Its direct messages to people who share a community with you stop being accepted once the removal syncs; blocking messages to contacts you only DM directly lands in follow-up work.
```

- [ ] **Step 4: Run vitest + tsc to verify pass**

Run (repo root): `npx vitest run src/lib/components/__tests__/RemoveDeviceDialog.test.ts && npx tsc --noEmit`
Expected: PASS.

- [ ] **Step 5: Update the ZEB-668 honesty ledger** (`docs/specs/2026-07-11-zeb-668-device-management-design.md`)

In §8 row `:297` (`…including DMs/vines/storage records`): update the Reality column to note DMs to shared-community contacts now block (ZEB-580 S2), DM-only contacts pending S3. In the Handling column change `DMs: ZEB-580.` → `DMs to shared-community contacts: ZEB-580 S2 (ZEB-684); DM-only contacts: S3 (ZEB-685).` In §9 (`:313`) update the parenthetical to reference the S2/S3 split.

- [ ] **Step 6: Commit**
```bash
git add src/lib/components/RemoveDeviceDialog.svelte src/lib/components/__tests__/RemoveDeviceDialog.test.ts docs/specs/2026-07-11-zeb-668-device-management-design.md
git commit -m "ZEB-580 S2 (T6): narrow device-remove DM honesty copy to community-scoped cutoff"
```

---

## Task 7: Edge-#5 verification — materialize does not expiry-filter DeviceRetire

**Files:**
- Modify: `src-tauri/src/community_membership.rs` (test module) — a unit test.

**Context (spec §8.5):** DM identity verify is expiry-agnostic, so an expired-but-not-revoked device still DMs (intended). The hazard: if some path filters expired enrollments before the DeviceRetire materializes, a revocation of an already-expired cert could be dropped and the projection never learn it. The `DeviceRetire` materialize arm (`community_membership.rs:2837-2843`) inserts unconditionally today. This task PINS that: a DeviceRetire whose cert is expired at materialize time still lands the key in `revoked_device_keys`.

- [ ] **Step 1: Write the test**

```rust
#[test]
fn device_retire_materializes_revocation_even_for_expired_cert() {
    // Build a community log with a member, then a DeviceRetire whose enrollment
    // cert is already past expiry at `now`. materialize_with_now(log, admin, now)
    // must still insert the retired ed25519 into revoked_device_keys.
    // (Reuse the existing DeviceRetire materialize test fixture; set now well past
    //  the cert's not_after.)
    let (log, admin, retired_vk, now_past_expiry) = device_retire_expired_fixture();
    let mat = materialize_with_now(&log, admin, now_past_expiry);
    let member = mat.members.get(&admin).expect("member present");
    assert!(
        member.revoked_device_keys.contains(&retired_vk),
        "an expired-cert DeviceRetire must still record the revocation (spec §8.5)"
    );
}
```

- [ ] **Step 2: Run it.**

Run: `cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(device_retire_materializes_revocation_even_for_expired)'`
- If it PASSES: the materialize path is expiry-agnostic (expected). Keep the test as a pin. Done.
- If it FAILS: the materialize path DROPS expired retires → this is a real gap. Do NOT fix it in this PR (out of S2 scope); record it in the progress ledger and the PR description as a discovered residual and **file a follow-up ticket** (per standing rules — never fold unrelated fixes). Adjust the test to document the actual behavior (`#[ignore]` with a `// FIXME(ZEB-xxx)` pointing at the follow-up) so the branch stays green.

- [ ] **Step 3: Commit**
```bash
cd src-tauri && cargo fmt --all
git add src-tauri/src/community_membership.rs
git commit -m "ZEB-580 S2 (T7): pin DeviceRetire materialize is expiry-agnostic (spec §8.5)"
```

---

## Task 8: Integration e2e — revoked DM dropped, non-revoked delivered + full sweep

**Files:**
- Create: `src-tauri/tests/dm/dm_revocation_cutoff_integration.rs`
- Register in the `tests/dm/` harness module (mirror how `dm_cert_identity_integration.rs` is registered — check `tests/dm_tests.rs` or the `mod` wiring S1 used).

**Interfaces:** end-to-end; consumes the whole receive stack + the projection. Reuses the S1 `dm_cert_identity_integration.rs` two-node harness.

**Context:** S1 landed `tests/dm/dm_cert_identity_integration.rs` (real two-node iroh handshake + real `mint_owner`, driving the production `drain → dm_signing_material` send path and the receive path). This task adds the S2 assertion: a #2-signed DM from a device whose owner has revoked it (shared community) is dropped; a non-revoked device delivers.

- [ ] **Step 1: Write the e2e test**

Model on `dm_cert_identity_integration.rs::cert_anchored_dm_roundtrip_end_to_end`. Extend/clone it to:
1. Establish two nodes that share a community; friend-handshake so node B caches node A's #2.
2. Baseline: A sends a #2-signed CidNotify → B verifies, delivers (assert delivered).
3. Revoke A's device in the shared community (materialize a `DeviceRetire` for A's #2 into B's community state; drive B's projection feed — either via the real on-epoch hook or by calling `union_from_members` on B's projection handle with A's revoked ed25519).
4. A sends another #2-signed CidNotify → B's `verify_cidnotify_sender_binding` (through the real ingest path) returns `SignerDeviceRevoked`; assert NOT delivered, NOT acked.
5. Control: a DIFFERENT non-revoked #2 device delivers normally (projection is per-key).

```rust
// Skeleton — fill from the S1 harness helpers.
#[tokio::test]
async fn revoked_device2_dm_is_dropped_after_community_revocation() {
    // ... two-node setup + friend handshake (copy from S1 harness) ...
    // baseline deliver:
    // assert!(node_b_delivered_cidnotify(...).await);
    // revoke A's #2 in B's projection:
    // b_revoked.union_from_members(std::iter::once((a_owner, &BTreeSet::from([a_ed25519]))));
    // second send -> dropped:
    // let outcome = node_b_ingest_cidnotify(...).await;
    // assert!(matches!(outcome, Err(e) if e.contains("SignerDeviceRevoked") || /* drop */ ));
    // assert not delivered / not acked.
}
```

- [ ] **Step 2: Run the integration test**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(revoked_device2_dm_is_dropped)'`
Expected: PASS.

- [ ] **Step 3: Full CI-parity sweep** (final task only)

Run:
```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd /Users/zeblith/work/zeblithic/harmony-client && npx tsc --noEmit && npx vitest run
```
Expected: all green. Verify the retained S1/S2 pins pass (`sign_dm_packet_matches_private_identity_sign`, `derive_device_hash_equals_harmony_identity_address_hash`, the S1 cert-integration test).

- [ ] **Step 4: Commit**
```bash
git add -A
git commit -m "ZEB-580 S2 (T8): e2e revoked-DM-dropped integration + full sweep green"
```

---

## Notes for the executor

- **Discovery order matters for tests.** Tasks 3–5 reuse the *existing* happy-path fixtures for `verify_cidnotify_sender_binding` / `apply_invite` / `handle_ack`. Locate them first (`rg 'fn .*(verify_cidnotify|apply_invite|handle_ack)' src-tauri/src/dm_outbox.rs`); the new cutoff tests differ from the happy path ONLY by the `revoked` argument, so factor a shared fixture rather than rebuilding state.
- **The param-threading is the bulk of the mechanical work.** Adding `revoked` to three helpers ripples to every call site (live + dormant + tests). Use `rg` to enumerate all sites per task and pass `&RevokedDeviceProjection::new()` where revocation isn't under test (empty ⇒ unchanged behavior).
- **Sticky feed has no remove branch** (unlike `MembershipProjection`) — feed on every delta, never retract.
- **Do not hold the projection's `RwLock` across an `.await`** — `is_revoked`/`union_from_members` are synchronous and return immediately; that is intentional.
