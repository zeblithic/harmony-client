# ZEB-236 DM-Invite Accept/Decline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop silently auto-accepting DM invites from non-friends — stage them for a user decision (toast + pending list), keep active-friend invites auto-accepting.

**Architecture:** A tier fork inside `apply_invite` (the single verified-invite apply point): active-friend inviters run the existing accept tail unchanged; others return a `Staged` outcome that callers record in a new process-local `PendingDmInvites` store and announce via `NodeEventSink` events. New three-layer verbs (`list_pending_dm_invites` / `accept_dm_invite` / `decline_dm_invite`) mirror the friend-request trio; the frontend mirrors `friend-service.ts` + `FriendsPanel` and adds a corner toast. Spec: `docs/specs/2026-07-04-zeb-236-dm-invite-decline-design.md`.

**Tech Stack:** Rust (Tauri 2 backend, `src-tauri/`), Svelte 5 + TypeScript frontend, cargo-nextest, vitest.

## Global Constraints

- Cargo commands run from `src-tauri/`, ONE cargo invocation at a time.
- Per-task Rust tests: `cargo nextest run --locked -p harmony-app --features test-fixtures -E '<filter>'` — do NOT use `--all-targets` per-task (≈97-binary relink cost); the final sweep task does that once.
- Lint gates (final task, CI form): `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` and `cargo fmt --all -- --check`.
- Frontend gates from repo root: `npx tsc --noEmit`, `npx vitest run`.
- Tauri IPC: Rust params `snake_case`, JS callers `camelCase` (auto-converted).
- Frontend error extraction: `e instanceof Error ? e.message : String(e)`.
- Never construct `KeychainStore::new()` in test-reachable code; tests set `HARMONY_PASSPHRASE`.
- Decline contract (spec): decline writes NO persistent state anywhere and never notifies the inviter.
- Event names (exact): `dm-invite-received` (new staging only), `dm-invite-list-changed` (any store mutation).
- Commit after every task. No worktrees.

---

### Task 1: `PendingDmInvites` store + `NodeState` slot

**Files:**
- Create: `src-tauri/src/pending_dm_invites.rs`
- Modify: `src-tauri/src/lib.rs` (module decl next to `mod friend_requests;`; `NodeState` field near `pending_friend_requests` at ~`lib.rs:1397`; construction where `PendingFriendRequests` is built at ~`lib.rs:3818`; the reset sites at ~`lib.rs:1536` and ~`lib.rs:1761`; the start_node wiring at ~`lib.rs:9708` — grep `pending_friend_requests` and mirror every site)

**Interfaces:**
- Consumes: `crate::dm_envelope::DmInviteSigned` (Clone + Serialize), `crate::owner_state_types::SpaceId`.
- Produces (later tasks rely on these exact names):
  - `pub struct StagedDmInvite { pub signed: DmInviteSigned, pub received_at_ms: u64, pub refresh_owner_device_cache: bool }`
  - `pub struct PendingDmInvites` with `pub fn new() -> Self`, `pub fn stage(&self, staged: StagedDmInvite) -> bool` (false = already pending, keep-first), `pub fn list(&self) -> Vec<StagedDmInvite>`, `pub fn take(&self, space_id: &SpaceId) -> Option<StagedDmInvite>`.
  - `NodeState.pending_dm_invites: Option<std::sync::Arc<PendingDmInvites>>`.

- [ ] **Step 1: Write the module with failing-first tests**

Create `src-tauri/src/pending_dm_invites.rs` (module doc mirrors `friend_requests.rs`'s process-local/ephemeral rationale — ZEB-483 co-deposits every invite alongside each message CidNotify, so a restart-lost pending invite re-stages on the next inbound message; ephemerality is also what keeps the decline contract pure):

```rust
//! ZEB-236: process-local pending inbound DM-invite store.
//!
//! A verified `DmInvite` from a NON-active-friend inviter is staged here (not
//! applied) until the user explicitly accepts or declines. PROCESS-LOCAL and
//! deliberately ephemeral (mirrors `friend_requests::PendingFriendRequests`):
//! ZEB-483 co-deposits the rebuilt invite alongside every message CidNotify,
//! so an entry lost to a restart re-stages on the next inbound message, and
//! keeping nothing on disk is what makes decline write no persistent state
//! (spec §"DmInvite rejection / decline semantics (v1)").

use crate::dm_envelope::DmInviteSigned;
use crate::owner_state_types::SpaceId;
use std::collections::HashMap;
use std::sync::Mutex;

/// One verified, staged DM invite awaiting the user's decision. Carries
/// everything the deferred accept needs to run the exact tail auto-accept
/// runs today.
#[derive(Debug, Clone)]
pub struct StagedDmInvite {
    /// The signature-verified invite (verified at staging time by
    /// `apply_invite`'s gates — accept does NOT re-verify).
    pub signed: DmInviteSigned,
    /// Wall-clock epoch-ms first staged (idempotent: redelivery keeps this).
    pub received_at_ms: u64,
    /// The ingest route's cache-refresh entitlement (tunnel=true,
    /// deposit-recover=false). Accept must honor the same trust distinction
    /// the auto-accept path applies (ZEB-483).
    pub refresh_owner_device_cache: bool,
}

/// Process-local store of staged DM invites, keyed by `SpaceId`. Single
/// `Mutex` held only for the duration of one map op — never across `.await`.
#[derive(Default)]
pub struct PendingDmInvites {
    inner: Mutex<HashMap<SpaceId, StagedDmInvite>>,
}

impl PendingDmInvites {
    /// Empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Stage an invite. Returns `true` if newly staged; `false` when an
    /// invite for the same `space_id` is already pending (keep-first — a
    /// ZEB-483 co-deposit redelivery must NOT bump `received_at_ms`, and the
    /// caller must NOT re-emit `dm-invite-received` for it).
    pub fn stage(&self, staged: StagedDmInvite) -> bool {
        let mut inner = self.inner.lock().expect("pending dm invites poisoned");
        match inner.entry(staged.signed.space_id) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(staged);
                true
            }
        }
    }

    /// Snapshot the currently-pending invites (for the list IPC).
    pub fn list(&self) -> Vec<StagedDmInvite> {
        self.inner
            .lock()
            .expect("pending dm invites poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// Remove + return the staged invite for `space_id` (accept and decline
    /// both consume through here; decline simply drops the return).
    pub fn take(&self, space_id: &SpaceId) -> Option<StagedDmInvite> {
        self.inner
            .lock()
            .expect("pending dm invites poisoned")
            .remove(space_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build a minimal DmInviteSigned fixture. Reuse the field layout from
    // dm_envelope.rs:67-115; values are arbitrary but self-consistent
    // (inviter ∈ members not required here — store logic is gate-agnostic).
    fn staged(space: u8, ms: u64) -> StagedDmInvite {
        StagedDmInvite {
            signed: crate::dm_envelope::test_fixtures::minimal_invite_for_space(space),
            received_at_ms: ms,
            refresh_owner_device_cache: true,
        }
    }

    #[test]
    fn stage_then_list_then_take() {
        let store = PendingDmInvites::new();
        assert!(store.stage(staged(1, 100)));
        assert_eq!(store.list().len(), 1);
        let took = store.take(&store.list()[0].signed.space_id);
        assert!(took.is_some());
        assert!(store.list().is_empty());
        assert!(store.take(&took.unwrap().signed.space_id).is_none());
    }

    #[test]
    fn stage_is_idempotent_keep_first() {
        let store = PendingDmInvites::new();
        assert!(store.stage(staged(2, 100)));
        // Redelivery: same space_id, later timestamp — must be rejected and
        // must NOT bump received_at_ms.
        assert!(!store.stage(staged(2, 999)));
        let list = store.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].received_at_ms, 100);
    }

    #[test]
    fn decline_then_redeliver_restages() {
        let store = PendingDmInvites::new();
        assert!(store.stage(staged(3, 100)));
        let sid = store.list()[0].signed.space_id;
        store.take(&sid); // decline consumes
        // The next ZEB-483 redelivery re-stages (spec: repeat invites re-prompt).
        assert!(store.stage(staged(3, 200)));
        assert_eq!(store.list()[0].received_at_ms, 200);
    }
}
```

Fixture note: if `crate::dm_envelope::test_fixtures::minimal_invite_for_space` does not exist, add a `#[cfg(test)] pub mod test_fixtures` to `dm_envelope.rs` with that constructor (derive the field values from the existing invite-building test fixture used by `handle_invite_writes_space_and_cache_with_signing_pub`, `dm_outbox.rs:~5588` — one function, `space: u8` seeds `space_id`/`inviter`/`members`/`sender_devices` deterministically). Task 2's tests reuse it.

- [ ] **Step 2: Wire the module + NodeState slot**

In `src-tauri/src/lib.rs`:
- Add `mod pending_dm_invites;` beside `mod friend_requests;`.
- Add the field directly under `pending_friend_requests` (~`:1397`):

```rust
    /// ZEB-236: process-local staged DM invites awaiting user accept/decline.
    /// Same lifecycle as `pending_friend_requests`.
    pub(crate) pending_dm_invites: Option<std::sync::Arc<crate::pending_dm_invites::PendingDmInvites>>,
```

- Mirror EVERY `pending_friend_requests` lifecycle site (grep `pending_friend_requests` in `lib.rs`): construction (`Some(Arc::new(PendingDmInvites::new()))` next to ~`:3818`), the reset-to-`None` sites (~`:1536`, ~`:1761`), and the start_node guard wiring (~`:9708`). Struct-literal initializers that list `pending_friend_requests: None` get `pending_dm_invites: None` too (the compiler will point at every missing site).

- [ ] **Step 3: Run the store tests**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(pending_dm_invites)'`
Expected: 3 PASS.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/pending_dm_invites.rs src-tauri/src/lib.rs src-tauri/src/dm_envelope.rs
git commit -m "ZEB-236 T1: process-local PendingDmInvites store + NodeState slot"
```

---

### Task 2: tier fork in `apply_invite` + extracted accept tail

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs:2217-2380` (`apply_invite`), `:1793-1823` (`handle_invite`)
- Test: same file's `#[cfg(test)]` mod (beside `handle_invite_writes_space_and_cache_with_signing_pub` at ~`:5588`)

**Interfaces:**
- Consumes: `crate::pending_dm_invites::StagedDmInvite` (Task 1).
- Produces:
  - `pub(crate) enum ApplyInviteOutcome { Accepted, Staged(crate::pending_dm_invites::StagedDmInvite) }`
  - `apply_invite(...) -> Result<ApplyInviteOutcome, DmReceiveError>` (same params).
  - `pub(crate) fn run_invite_accept_tail(state: &mut OwnerState, device_id: &str, signed: crate::dm_envelope::DmInviteSigned, wall_now_ms: u64, refresh_owner_device_cache: bool) -> Result<(), DmReceiveError>` — Task 4's `accept_dm_invite_impl` calls this exact function.

- [ ] **Step 1: Extract the accept tail**

Move the ENTIRE existing block from the `// Phase 3b auto-accept` comment (`:2279`) through the cache-refresh `if` (`:2377`) into a new free fn placed directly after `apply_invite`, byte-for-byte except the signature line — the Space literal, `apply_space_with_canonicalization` + rejection check, and the whole gated `refresh_owner_device_cache` block including all SECURITY comments:

```rust
/// ZEB-236: the invite ACCEPT tail — exactly the Phase 3b auto-accept body,
/// extracted so the deferred user-accept path (`accept_dm_invite_impl`) and
/// the friend-tier auto-accept run the same code. Callers guarantee `signed`
/// already passed `apply_invite`'s gates + signature verification.
pub(crate) fn run_invite_accept_tail(
    state: &mut OwnerState,
    device_id: &str,
    signed: crate::dm_envelope::DmInviteSigned,
    wall_now_ms: u64,
    refresh_owner_device_cache: bool,
) -> Result<(), DmReceiveError> {
    // <moved body, unchanged: Space literal → apply_space_with_canonicalization
    //  → gated OwnerDeviceCache refresh>
    Ok(())
}
```

- [ ] **Step 2: Fork `apply_invite`**

Replace the moved body inside `apply_invite` (everything after the signature verification at `:2277`) with:

```rust
    // ZEB-236 tier fork: invites from ACTIVE friends keep Phase 3b's
    // auto-accept (the friendship approval was the consent gate). Anything
    // else is STAGED for an explicit user decision — no Space, no cache
    // write, nothing persistent (spec: decline must be indistinguishable
    // from offline, so staging itself is process-local only).
    let inviter_is_active_friend = state
        .friend_graph
        .friends
        .get(&signed.inviter)
        .is_some_and(|e| e.status == crate::friend_graph::FriendStatus::Active);
    if !inviter_is_active_friend {
        return Ok(ApplyInviteOutcome::Staged(
            crate::pending_dm_invites::StagedDmInvite {
                signed,
                received_at_ms: wall_now_ms,
                refresh_owner_device_cache,
            },
        ));
    }
    run_invite_accept_tail(state, device_id, signed, wall_now_ms, refresh_owner_device_cache)?;
    Ok(ApplyInviteOutcome::Accepted)
```

Change the return type to `Result<ApplyInviteOutcome, DmReceiveError>` and delete the now-unused `DrainOutcome` import if orphaned. Update the dormant `handle_invite` (`:1793`) to match: on `Ok(ApplyInviteOutcome::Staged(_))` it logs `tracing::warn!("dormant handle_invite: non-friend invite staged-and-dropped (no store on this path)")` and returns `Ok(DrainOutcome::default())`; on `Accepted` likewise `Ok(DrainOutcome::default())`.

- [ ] **Step 3: Write the tier/parity/purity tests**

In the existing `#[cfg(test)]` mod, cloning the fixture setup from `handle_invite_writes_space_and_cache_with_signing_pub` (~`:5588`) — same invite builder, same `OwnerState` harness. Four tests:

```rust
#[test]
fn apply_invite_from_active_friend_auto_accepts() {
    // fixture: state where signed.inviter is in friend_graph with
    // FriendStatus::Active (insert a FriendEntry the way lib.rs:46672 does).
    // assert: Ok(ApplyInviteOutcome::Accepted); state.spaces contains
    // signed.space_id; cache row present (refresh=true variant).
}

#[test]
fn apply_invite_from_non_friend_stages_and_writes_nothing() {
    // fixture: same invite, friend_graph EMPTY. Capture canonical CBOR bytes
    // of the OwnerState BEFORE (owner_state_persist::canonicalize or the
    // existing test-mod helper). assert: Ok(Staged(s)) with
    // s.signed.space_id == invite's, s.refresh_owner_device_cache == passed
    // flag; canonical bytes AFTER are IDENTICAL (nothing written).
}

#[test]
fn staged_then_accept_tail_matches_direct_auto_accept_golden() {
    // Two identical OwnerStates A and B (inviter Active in both).
    // A: apply_invite(...) direct → Accepted.
    // B: make inviter non-friend first → Staged(s); then re-add Active is NOT
    //    needed — call run_invite_accept_tail(B, device_id, s.signed,
    //    same wall_now_ms, s.refresh_owner_device_cache).
    // assert canonicalize(A) == canonicalize(B) — for BOTH
    // refresh_owner_device_cache variants (parameterize or write two tests).
}

#[test]
fn decline_writes_no_state() {
    // The reinstated spec test: non-friend invite → Staged(s); DROP s (that
    // is all decline does at this layer). assert canonical OwnerState bytes
    // unchanged from pre-invite snapshot.
}
```

- [ ] **Step 4: Fix call sites enough to compile, run the dm_outbox tests**

The two live callers (`dm_inbox_ingest.rs:472`, `community_relay_prod.rs:427`) now get a `Result<ApplyInviteOutcome, _>`; for THIS task make them compile by treating `Staged(_)` as a logged drop (`tracing::info!("ZEB-236: non-friend DM invite staged pending store wiring (T3)")`) — Task 3 replaces that with real staging. `apply_deposited_invite` keeps returning `Result<(), String>` for now (maps both outcomes to `Ok(())` with the same interim log).

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(apply_invite) or test(decline_writes_no_state) or test(handle_invite)'`
Expected: new tests PASS; `handle_invite_writes_space_and_cache_with_signing_pub` FAILS if its fixture inviter isn't an Active friend — update that existing test to insert the Active friend entry (its intent is the accept path; the tier is new, the update is the honest fix) — then PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/dm_outbox.rs src-tauri/src/dm_inbox_ingest.rs src-tauri/src/community_relay_prod.rs
git commit -m "ZEB-236 T2: tier fork in apply_invite + extracted run_invite_accept_tail (golden parity pinned)"
```

---

### Task 3: call-site staging + events

**Files:**
- Modify: `src-tauri/src/dm_inbox_ingest.rs` (invite arm `:448-486` + `ingest_dm_packet` signature + its callers), `src-tauri/src/community_relay_prod.rs` (struct fields + `:427-438` + `apply_deposited_invite` handling `:495-508`), `src-tauri/src/dm_outbox.rs` (`apply_deposited_invite` `:2399` return type → `Result<Option<crate::pending_dm_invites::StagedDmInvite>, String>`), `src-tauri/src/api/rpc.rs` (WS event allowlist ~`:1457-1464`)

**Interfaces:**
- Consumes: `ApplyInviteOutcome`, `PendingDmInvites::stage`, `crate::node_event_sink::{NodeEventSink, emit_ser}`.
- Produces: on every live ingest route, a non-friend invite ends up staged in `NodeState.pending_dm_invites` with `dm-invite-received` emitted ONLY when `stage()` returned true, and `dm-invite-list-changed` emitted whenever staging succeeded.

- [ ] **Step 1: Thread store + sink into the tunnel ingest**

Add two params to `ingest_dm_packet` (and thread from its caller(s) — grep `ingest_dm_packet(`; the tunnel-acceptor caller already owns the sink it uses for `dm-received` emission and can clone the `Arc<PendingDmInvites>` out of `NodeState` at the same place it snapshots `crdt_state`):

```rust
    pending_invites: Option<std::sync::Arc<crate::pending_dm_invites::PendingDmInvites>>,
    event_sink: Option<std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>>,
```

In the `DmPacket::Invite` arm, replace the interim log from Task 2 with:

```rust
            match crate::dm_outbox::apply_invite(/* unchanged args */)? {
                crate::dm_outbox::ApplyInviteOutcome::Accepted => {}
                crate::dm_outbox::ApplyInviteOutcome::Staged(staged) => {
                    if let Some(pending) = pending_invites.as_ref() {
                        let newly = pending.stage(staged);
                        if let Some(sink) = event_sink.as_ref() {
                            if newly {
                                crate::node_event_sink::emit_ser(
                                    sink.as_ref(), "dm-invite-received", &(),
                                );
                            }
                            crate::node_event_sink::emit_ser(
                                sink.as_ref(), "dm-invite-list-changed", &(),
                            );
                        }
                    } else {
                        tracing::warn!(
                            "ZEB-236: staged DM invite dropped (pending store not wired on this path)"
                        );
                    }
                }
            }
            return Ok(false);
```

(Payload-less events: the frontend re-fetches via `list_pending_dm_invites`, mirroring `friend-list-changed`'s `&()` payload at `lib.rs:48793`.)

- [ ] **Step 2: Same for the relay/deposit paths**

`community_relay_prod.rs`: add `pending_dm_invites: Option<Arc<crate::pending_dm_invites::PendingDmInvites>>` and `event_sink: Option<Arc<dyn crate::node_event_sink::NodeEventSink>>` fields to the struct holding `crdt_state`/`self_owner`/`device_id`; populate at its construction site(s) (grep the struct name's `::new`/literal in `lib.rs`) from the same `NodeState` the constructor already reads. Invite-only arm (`:427`) and the co-deposited arm both handle outcomes with the identical stage+emit block from Step 1. `apply_deposited_invite` (`dm_outbox.rs:2399`) now returns `Ok(Some(staged))` on `Staged` / `Ok(None)` on `Accepted`; its caller (`:495-508`) runs the stage+emit block on `Some` AFTER the `crdt_state` lock is released (the store mutex must not nest inside an `.await`-holding lock scope longer than needed — take the value out, drop the guard, then stage+emit).

- [ ] **Step 3: WS event allowlist**

`src-tauri/src/api/rpc.rs` (~`:1457-1464`, the friends block): add `"dm-invite-received"` and `"dm-invite-list-changed"` to the event-name allowlist, same style as the friend entries.

- [ ] **Step 4: Compile + targeted tests**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(ingest) or test(relay) or test(apply_invite)'`
Expected: PASS (existing ingest/relay tests updated only where the new params need `None, None` in test callers).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src
git commit -m "ZEB-236 T3: stage non-friend invites at all live ingest routes + dm-invite events"
```

---

### Task 4: the verb trio (`list` / `accept` / `decline`)

**Files:**
- Modify: `src-tauri/src/lib.rs` (new block directly after the friend-request trio ending ~`:48810`; `generate_handler!` list ~`:52174`), `src-tauri/src/api/rpc.rs` (registrations beside `accept_friend_request` at `:739-746`)
- Test: `lib.rs` test mod (projector test beside the friend DTO tests) + `dm_outbox.rs` accept-parity already pinned in Task 2

**Interfaces:**
- Consumes: Task 1 store API, Task 2 `run_invite_accept_tail`.
- Produces: verbs `list_pending_dm_invites` (no args → `Vec<PendingDmInviteDto>`), `accept_dm_invite { space_id: String }`, `decline_dm_invite { space_id: String }`; `pub struct PendingDmInviteDto` (`camelCase`): `space_id_hex: String`, `inviter_owner_id_hex: String`, `kind: crate::owner_state_types::SpaceKind`, `member_owner_ids_hex: Vec<String>`, `created_at_ms: u64`, `received_at_ms: u64`.

- [ ] **Step 1: DTO + pure projector + unit test**

Mirror `PendingFriendRequestDto` / `list_pending_friend_requests_inner` (`lib.rs:48697-48722`) exactly:

```rust
/// ZEB-236: one staged inbound DM invite surfaced to the frontend. Deliberately
/// projects ONLY routing/display fields — never `content_key` or
/// `inviter_identity_pub` (trust-secret material stays backend-side).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingDmInviteDto {
    pub space_id_hex: String,
    pub inviter_owner_id_hex: String,
    pub kind: crate::owner_state_types::SpaceKind,
    pub member_owner_ids_hex: Vec<String>,
    pub created_at_ms: u64,
    pub received_at_ms: u64,
}

/// Pure projector (unit-testable without a NodeState harness).
pub fn list_pending_dm_invites_inner(
    store: &crate::pending_dm_invites::PendingDmInvites,
) -> Vec<PendingDmInviteDto> {
    let mut rows: Vec<PendingDmInviteDto> = store
        .list()
        .into_iter()
        .map(|s| PendingDmInviteDto {
            space_id_hex: hex::encode(s.signed.space_id.0),
            inviter_owner_id_hex: hex::encode(s.signed.inviter.0),
            kind: s.signed.kind,
            member_owner_ids_hex: s.signed.members.iter().map(|m| hex::encode(m.0)).collect(),
            created_at_ms: s.signed.created_at.wall_ms,
            received_at_ms: s.received_at_ms,
        })
        .collect();
    rows.sort_by_key(|r| r.received_at_ms); // deterministic list order
    rows
}
```

(If `SpaceId`'s inner bytes aren't `pub .0`, use its existing hex helper — grep how `list_channels` DTOs encode `SpaceId`.) Unit test: build a store with one staged fixture invite, project, assert every field AND assert the serialized JSON (`serde_json::to_value`) has exactly the six camelCase keys — that pins the no-secret-material property.

- [ ] **Step 2: the three verbs**

Clone the friend-trio structure verbatim (`lib.rs:48736-48810`), adapting:

- `list_pending_dm_invites` / `_impl`: snapshot `state.lock().pending_dm_invites.clone()`, `None` → `Ok(Vec::new())`, else project.
- `accept_dm_invite(app, state, space_id: String)` / `_impl(state, sink, space_id)`:

```rust
pub(crate) async fn accept_dm_invite_impl(
    state: &std::sync::Mutex<NodeState>,
    sink: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink>,
    space_id_hex: String,
) -> Result<(), String> {
    let space_id = decode_space_id_16(&space_id_hex)?; // mirror decode_owner_id_16 (:48725)
    let (store, device_id) = {
        let g = state.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        (g.pending_dm_invites.clone(), g.device_id.clone()) // use however NodeState exposes the local device id — grep device_id around lib.rs:3818 / how apply_invite callers obtain it
    };
    let Some(store) = store else {
        return Err(OWNER_NOT_LOADED_MSG.into());
    };
    let Some(staged) = store.take(&space_id) else {
        return Err("no pending DM invite for space".into());
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    {
        let mut g = state.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        let owner_state = /* the same &mut OwnerState access the friend verbs use on NodeState — grep how accept-path verbs reach crdt state under this lock */;
        crate::dm_outbox::run_invite_accept_tail(
            owner_state, &device_id, staged.signed, now_ms, staged.refresh_owner_device_cache,
        )
        .map_err(|e| format!("accept failed: {e:?}"))?;
        // persist via the same save path other OwnerState-mutating verbs use here
    }
    crate::node_event_sink::emit_ser(sink.as_ref(), "dm-invite-list-changed", &());
    crate::node_event_sink::emit_ser(sink.as_ref(), "nav-updated", &()); // the new Space must appear
    Ok(())
}
```

  IMPORTANT correctness note for the implementer: whatever lock/persist pattern the sibling OwnerState-mutating IPCs in this file use (e.g. `add_space`) — apply/persist/flush — copy it exactly; the accepted Space must both persist and replicate the same way an `add_space` Space does. If the accept-tail apply fails, the staged invite was already consumed — RE-STAGE it (`store.stage(staged_clone)`) before returning the error so a transient failure isn't a silent decline (clone `staged` before the take-consume... simpler: clone before calling the tail).
- `decline_dm_invite` / `_impl`: `store.take(&space_id)` → `None` is `Err("no pending DM invite for space")`; on `Some`, DROP it, emit ONLY `dm-invite-list-changed`. Doc-comment cites the spec contract (no state, no inviter notification).

- [ ] **Step 3: registrations**

- `generate_handler!` (~`:52174`): add the three names beside the friend trio.
- `rpc.rs` (`:739-746` style): `EmptyArgs` for list; new `SpaceIdHexArgs { space_id: String }` args struct beside `OwnerIdHexArgs` (`:276`) for accept/decline; `rpc!` entries delegating to the `_impl`s.

- [ ] **Step 4: Test + run**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(pending_dm_invite) or test(dm_invite)'`
Expected: projector + store + Task-2 suite PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/api/rpc.rs
git commit -m "ZEB-236 T4: list/accept/decline DM-invite verbs (Tauri + headless rpc), DTO projector"
```

---

### Task 5: `dm-invite-service.ts`

**Files:**
- Create: `src/lib/dm-invite-service.ts`
- Test: `src/lib/dm-invite-service.test.ts`

**Interfaces:**
- Consumes: `TauriAdapter` (`src/lib/zenoh-service.ts:2-5`), `createMockAdapter` (`src/lib/test-utils.ts`).
- Produces: `class DmInviteService` with `connectAdapter(adapter)`, `listPending(): Promise<PendingDmInviteDto[]>`, `accept(spaceIdHex: string)`, `decline(spaceIdHex: string)`, `onPendingChanged(cb): () => void`, `destroy()`; `export interface PendingDmInviteDto { spaceIdHex: string; inviterOwnerIdHex: string; kind: string; memberOwnerIdsHex: string[]; createdAtMs: number; receivedAtMs: number }`.

- [ ] **Step 1: Write the failing tests** — mirror `friend-service` conventions with `createMockAdapter`:

```typescript
import { describe, expect, it, vi } from 'vitest';
import { DmInviteService } from './dm-invite-service';
import { createMockAdapter } from './test-utils';

describe('DmInviteService', () => {
  it('fans out onPendingChanged for both invite events', async () => {
    const { adapter, emit } = createMockAdapter();
    const svc = new DmInviteService();
    await svc.connectAdapter(adapter);
    const cb = vi.fn();
    svc.onPendingChanged(cb);
    emit('dm-invite-received', {});
    emit('dm-invite-list-changed', {});
    expect(cb).toHaveBeenCalledTimes(2);
  });

  it('listPending invokes the verb and returns rows', async () => {
    const { adapter } = createMockAdapter();
    (adapter.invoke as any) = vi.fn().mockResolvedValue([
      { spaceIdHex: 'aa', inviterOwnerIdHex: 'bb', kind: 'dm',
        memberOwnerIdsHex: ['bb', 'cc'], createdAtMs: 1, receivedAtMs: 2 },
    ]);
    const svc = new DmInviteService();
    await svc.connectAdapter(adapter);
    const rows = await svc.listPending();
    expect(adapter.invoke).toHaveBeenCalledWith('list_pending_dm_invites', {});
    expect(rows[0].inviterOwnerIdHex).toBe('bb');
  });

  it('accept/decline pass camelCase spaceId and normalize errors', async () => {
    const { adapter } = createMockAdapter();
    (adapter.invoke as any) = vi.fn().mockRejectedValue(new Error('no pending DM invite for space'));
    const svc = new DmInviteService();
    await svc.connectAdapter(adapter);
    await expect(svc.accept('aa')).rejects.toThrow('no pending DM invite for space');
    expect(adapter.invoke).toHaveBeenCalledWith('accept_dm_invite', { spaceId: 'aa' });
  });
});
```

- [ ] **Step 2: Implement** — structural clone of `friend-service.ts:161-252`: `connectAdapter` registers both listeners into tracked unlisteners, a `Set<() => void>` registry, `destroy()` tears down; `private call(cmd, args)` wrapper with `e instanceof Error ? e.message : String(e)` rethrow.

- [ ] **Step 3: Run** `npx vitest run src/lib/dm-invite-service.test.ts` — PASS; `npx tsc --noEmit` — clean.

- [ ] **Step 4: Commit** `git add src/lib/dm-invite-service.ts src/lib/dm-invite-service.test.ts && git commit -m "ZEB-236 T5: dm-invite-service (events + verbs mirror of friend-service)"`

---

### Task 6: `DmInviteToast` + App mount

**Files:**
- Create: `src/lib/components/DmInviteToast.svelte`
- Modify: `src/App.svelte` (service instantiation + adapter wiring beside the friend service; toast mount beside `IncomingCallToast` at ~`:2874`)
- Test: `src/lib/components/__tests__/DmInviteToast.test.ts`

**Interfaces:**
- Consumes: `DmInviteService` (Task 5).
- Produces: `<DmInviteToast invite={PendingDmInviteDto} onAccept onDecline onLater />` — pure presentational; App owns the queue (show oldest pending not yet dismissed-this-session; `dm-invite-received` → refresh list → surface newest-un-dismissed).

- [ ] **Step 1: Failing test** (pattern of `ConfirmDialog.test.ts` / `DmCreateDialog.test.ts:26-60`):

```typescript
import { render, fireEvent } from '@testing-library/svelte';
import { describe, expect, it, vi } from 'vitest';
import DmInviteToast from '../DmInviteToast.svelte';

const invite = { spaceIdHex: 'a1', inviterOwnerIdHex: 'deadbeefdeadbeefdeadbeefdeadbeef',
  kind: 'dm', memberOwnerIdsHex: [], createdAtMs: 1, receivedAtMs: 2 };

describe('DmInviteToast', () => {
  it('renders inviter short-hex + kind and fires the three callbacks', async () => {
    const onAccept = vi.fn(); const onDecline = vi.fn(); const onLater = vi.fn();
    const { getByText, getByTestId } = render(DmInviteToast, {
      props: { invite, onAccept, onDecline, onLater },
    });
    expect(getByText(/deadbeef/)).toBeTruthy();   // short-hex display
    await fireEvent.click(getByTestId('dm-invite-accept'));
    await fireEvent.click(getByTestId('dm-invite-decline'));
    await fireEvent.click(getByTestId('dm-invite-later'));
    expect(onAccept).toHaveBeenCalledOnce();
    expect(onDecline).toHaveBeenCalledOnce();
    expect(onLater).toHaveBeenCalledOnce();
  });
});
```

- [ ] **Step 2: Component** — corner card visually consistent with `IncomingCallToast` (reuse its container/button classes or CSS tokens; consult that component's markup): title `DM invite`, body `From {invite.inviterOwnerIdHex.slice(0, 8)}… ({invite.kind})`, three buttons with the `data-testid`s above. Accept/Decline disable-while-in-flight (local `busy` state; callbacks are async).

- [ ] **Step 3: App wiring** — instantiate `DmInviteService` beside the friend service, `connectAdapter` in the same `isTauri()`-gated block (`App.svelte:1633-1648`); state `let dmInviteQueue: PendingDmInviteDto[]` refreshed via `onPendingChanged` → `listPending()`; mount beside `IncomingCallToast`:

```svelte
{#if dmInviteQueue.length > 0}
  <DmInviteToast
    invite={dmInviteQueue[0]}
    onAccept={() => dmInviteService.accept(dmInviteQueue[0].spaceIdHex)}
    onDecline={() => dmInviteService.decline(dmInviteQueue[0].spaceIdHex)}
    onLater={() => { laterDismissed.add(dmInviteQueue[0].spaceIdHex); dmInviteQueue = dmInviteQueue.filter(i => !laterDismissed.has(i.spaceIdHex)); }}
  />
{/if}
```

(`laterDismissed: Set<string>` is session-local; accepted/declined entries disappear via the `dm-invite-list-changed` refresh.)

- [ ] **Step 4: Run** `npx vitest run src/lib/components/__tests__/DmInviteToast.test.ts` + `npx tsc --noEmit` — PASS/clean.

- [ ] **Step 5: Commit** `git add src/lib/components/DmInviteToast.svelte src/lib/components/__tests__/DmInviteToast.test.ts src/App.svelte && git commit -m "ZEB-236 T6: DM-invite corner toast + App queue wiring"`

---

### Task 7: FriendsPanel "DM invites" pending section

**Files:**
- Modify: `src/lib/components/FriendsPanel.svelte` (new section directly below the friend-requests inbox `:881-937`; handlers beside `handleAccept`/`handleDecline` `:464-492`; subscription in `onMount` `:259-286`)
- Test: extend `src/lib/components/FriendsPanel.test.ts`

**Interfaces:**
- Consumes: `DmInviteService` — new optional prop `dmInviteService` on FriendsPanel (optional so existing instantiations/tests stay valid; section renders only when provided AND list non-empty).
- Produces: rows with `data-testid="dm-invite-accept-btn"` / `"dm-invite-decline-btn"`.

- [ ] **Step 1: Failing test** — extend `FriendsPanel.test.ts` with its `mockService()` pattern plus a mock dm-invite service (`{ listPending: vi.fn().mockResolvedValue([inviteRow]), accept: vi.fn(), decline: vi.fn(), onPendingChanged: vi.fn(() => () => {}) }`); render with both services; assert the row shows the short inviter hex and that clicking accept/decline calls the mock with `spaceIdHex` and refreshes.

- [ ] **Step 2: Implement** — clone the friend-request rows block (`:881-937`) structurally: heading `DM invites`, per-row inviter short-hex + kind + relative received time, Accept/Decline buttons with the same `requestInFlight`-style per-row guard (`:464-492` pattern keyed by `spaceIdHex`), `onMount` subscribes `dmInviteService?.onPendingChanged(refreshDmInvites)` and initial `refreshDmInvites()` (mirror `:259-286`).

- [ ] **Step 3: App passes the service** — `<FriendsPanel … dmInviteService={dmInviteService} />` at its existing mount.

- [ ] **Step 4: Run** `npx vitest run src/lib/components/FriendsPanel.test.ts` + `npx tsc --noEmit` — PASS/clean.

- [ ] **Step 5: Commit** `git add src/lib/components/FriendsPanel.svelte src/lib/components/FriendsPanel.test.ts src/App.svelte && git commit -m "ZEB-236 T7: DM-invites pending section in FriendsPanel"`

---

### Task 8: full gates + spec cross-check

- [ ] **Step 1:** `cd src-tauri && cargo fmt --all`
- [ ] **Step 2:** `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` — clean (CI form; catches `#[cfg(test)]` lints `--lib` misses).
- [ ] **Step 3:** `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast` — the ONE full sweep (expect ≥ ~4056 pass, 0 fail; >10 min → run with background supervision per repo convention).
- [ ] **Step 4:** repo root: `npx tsc --noEmit && npx vitest run` — clean / all pass.
- [ ] **Step 5:** Spec cross-check against `docs/specs/2026-07-04-zeb-236-dm-invite-decline-design.md`: every contract bullet (decline purity, tier, idempotent staging, DTO secrecy, headless parity, both events allowlisted) maps to a merged test or registration. Fix anything missing.
- [ ] **Step 6:** `git add -A && git commit -m "ZEB-236 T8: gates (fmt, clippy CI-form, full sweep, tsc, vitest)"`
