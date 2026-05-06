# ZEB-217 Sub-C Phase 3 — Open Community Flow Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **HARD RULES (per user memory):**
> - `cargo fmt --all -- --check` AND `cargo clippy --all-targets --all-features -- -D warnings` AND `cargo test --all-targets` must pass before every commit.
> - **No worktrees.** Use `git checkout -b` in the main repo. Never run `git worktree add` / `remove`.
> - **Branch must stay on origin/main lineage.** `git fetch origin && git checkout main && git pull origin main` before branching.
> - **Never invent Linear IDs.** This plan references existing IDs only (ZEB-217, ZEB-247, ZEB-249, ZEB-254, ZEB-256). If a new follow-up emerges during implementation, file the issue first, then reference it by the assigned ID.
> - **Pipe exit codes lie.** Use `set -o pipefail` or `${PIPESTATUS[0]}` when piping cargo through `tail`/`grep`. Don't trust naked `cmd | tail`.
> - **Test drift is our fault.** If a test on `main` breaks during this work, sweep + fix; don't externalize.
> - **Tauri IPC param naming:** Rust uses `snake_case`; Tauri 2 auto-converts JS `camelCase` → `snake_case` at the boundary. JS callers send `communityId`; Rust receives `community_id`. (PR #81 round 4 caught a `space_id_hex` vs `spaceId` bug — don't repeat it.)

**Goal:** Ship the open-community IPC layer end-to-end (no UI yet) — `create_community`, `redeem_invite` (open path), `leave_community`, `list_community_members`, `generate_invite` (open path) — backed by Phase 2's per-community CRDT + Zenoh sync, emitting `community-members-changed` on every local CRDT mutation (own append OR DAG-synced from peer).

**Architecture:** Phase 3 is a thin IPC layer over the shipped Phase 2 engine. The engine grows two new affordances: a `delta_tx` channel that fires `CommunityMembershipDelta` on every `InsertOutcome::Inserted` (covering both the receive-path inserts already in `handle_incoming_publish` AND a new `insert_local_event` method IPCs call to mint own events), and a Tauri-side consumer task (spawned at `start_node`) that owns the `AppHandle`, drains the delta channel, and emits `community-members-changed` events. IPCs that mutate community CRDT state inherit the snapshot-then-spawn fence hardened in PR #81 round 6 (`send_dm` / `add_space` / `delete_outbox_entry`) so a stop+restart racing through an in-flight IPC can't orphan-write to a detached state. Open invite URLs are `harmony://invite/{base64url(canonical_cbor(payload))}` strings returned to the caller for manual sharing — Phase 5 lands the deep-link plugin that makes them clickable.

**Tech Stack:** Rust 1.79+, tokio (async runtime), Tauri 2 (IPC), serde / ciborium (canonical CBOR), base64 0.22 (URL-safe-no-pad), thiserror (error taxonomy). Frontend: nothing in Phase 3 (vitest tests under `__tests__/` not exercised here; Phase 5 ships UI).

**Spec:** `docs/specs/2026-05-05-zeb-217-sub-c-communities-design.md` (commit `0b84296` on `main`). Phase 3 implements the "Open community redemption" flow (spec §"Invite system" + §"IPC surface").

**Branch:** `zeb-217-sub-c-phase3-open-community-flow` off latest `origin/main`.

---

## Scope check

Phase 3 ships **open communities only** end-to-end at the IPC layer. Out of scope:

- **Frontend / Svelte components** — Phase 5.
- **Invite-only redemption (Reticulum counter-sig hop)** — Phase 4. `redeem_invite` rejects payloads with `is_invite_only == true` for now.
- **`kick_from_community`, `set_power_level`, invite-only `generate_invite` (with signed `InviteToken`)** — Phase 4.
- **Deep-link plugin (`tauri-plugin-deep-link`)** — Phase 5; Phase 3's `generate_invite` returns the URL string, which the user shares manually (e.g., copy-paste into a chat).
- **Cryptographic publisher authentication on state-root publishes** — [ZEB-256](https://linear.app/zeblith/issue/ZEB-256/), required before Phase 4 ships. Phase 3 inherits the same open-only scope as Phase 2 so the spoof gap remains acceptable here.
- **TreeKEM-style backward secrecy on membership change** — [ZEB-249](https://linear.app/zeblith/issue/ZEB-249/).
- **Persistent offline-counter-signer queue** — [ZEB-254](https://linear.app/zeblith/issue/ZEB-254/).
- **End-to-end Tauri::invoke harness (ZEB-247)** — lands in Phase 5 alongside the UI.

The plan is one PR, ~14 tasks, each ending with a commit.

---

## File structure

**Files to create:**

- `src-tauri/tests/community_open_flow_integration.rs` — two-node integration test exercising create → invite → redeem → list-members → leave round-trip via the IPC inner functions.

**Files to modify:**

- `src-tauri/Cargo.toml` — add `base64 = "0.22"` to `[dependencies]`.
- `src-tauri/src/community_invite.rs` — add `encode_invite_url(payload) -> String` and `decode_invite_url(url) -> Result<CommunityInvitePayload, InviteUrlError>` helpers + new `InviteUrlError` enum.
- `src-tauri/src/community_state_sync.rs` — add `CommunityMembershipDelta` struct, extend `CommunitySyncEngineConfig` with `delta_tx: Option<mpsc::Sender<CommunityMembershipDelta>>`, plumb into `InternalCtx`, emit on every `Inserted` outcome inside `handle_incoming_publish`, add `pub async fn insert_local_event` method on `CommunitySyncEngine`.
- `src-tauri/src/lib.rs` — register five new IPCs (`create_community`, `redeem_invite`, `leave_community`, `list_community_members`, `generate_invite`); add DTO types (`MemberInfoDto`, `CommunityMembersChangedPayload`, `MembershipChange`); spawn the delta consumer task in `start_node`; extend `NodeState` with the delta channel sender (so IPC handlers and the consumer share the same channel); register five `tauri::generate_handler!` entries.

**Module-responsibility summary** (matches the spec's "New Rust modules" table — refresh adds `community_state_persist.rs` from Phase 2 and `community_invite.rs` URL helpers from this phase):

| Module | Phase 3 responsibility |
|---|---|
| `community_state_sync.rs` | Engine + registry (Phase 2). Phase 3 adds `CommunityMembershipDelta` + `delta_tx` + `insert_local_event`. |
| `community_invite.rs` | `CommunityInvitePayload` types (Phase 1). Phase 3 adds URL encode/decode + `InviteUrlError`. |
| `lib.rs` | IPC registration. Phase 3 adds five commands + DTOs + delta-consumer task wiring. |

---

## Task 0: Pre-flight — branch + baseline gates

**Files:** none modified.

- [ ] **Step 1: Sync main**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git fetch origin
git checkout main
git pull origin main
git log --oneline -3
```

Expected: tip is `0b84296 docs(zeb-217): refresh Sub-C spec against shipped Phase 2 + file ZEB-256 deferral (#85)`.

- [ ] **Step 2: Branch off main**

```bash
git checkout -b zeb-217-sub-c-phase3-open-community-flow
git status
```

Expected: `On branch zeb-217-sub-c-phase3-open-community-flow / nothing to commit, working tree clean`.

- [ ] **Step 3: Baseline cargo fmt + clippy + test**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
set -o pipefail
cargo test --all-targets 2>&1 | tail -5
```

Expected: all three exit `0`; final test summary shows `741 passed; 0 failed` (or higher if other PRs landed in between — the count must equal the count on `main` before this branch, ZERO failed).

- [ ] **Step 4: Baseline frontend gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
pnpm install
pnpm exec tsc --noEmit
pnpm exec vitest run
```

Expected: tsc clean (no errors); vitest all pass.

- [ ] **Step 5: No-op task — no commit**

Task 0 is verification-only. Move to Task 1 without committing.

---

## Task 1: `CommunityMembershipDelta` type + engine `delta_tx` plumbing (receive path)

**Files:**

- Modify: `src-tauri/src/community_state_sync.rs` — add `CommunityMembershipDelta` struct near the existing `CommunityDegradedReport` (~line 359); extend `CommunitySyncEngineConfig` (~line 376) with `delta_tx: Option<mpsc::Sender<CommunityMembershipDelta>>`; plumb into `InternalCtx`; emit on `Inserted` outcomes inside `handle_incoming_publish` (~line 1014).
- Modify: `src-tauri/tests/community_sync_engine_unit.rs` — add the new test from Step 1.

- [ ] **Step 1: Write the failing test**

Append to `src-tauri/tests/community_sync_engine_unit.rs`:

```rust
#[tokio::test]
async fn engine_emits_membership_delta_on_remote_insert() {
    use harmony_app::community_state_sync::CommunityMembershipDelta;
    use std::time::Duration;

    let (a_out_tx, mut a_out_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_a_in_tx, a_in_rx) = mpsc::channel::<Vec<u8>>(8);
    let (b_out_tx, _b_out_rx) = mpsc::channel::<Vec<u8>>(8);
    let (b_in_tx, b_in_rx) = mpsc::channel::<Vec<u8>>(8);
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel(64);
    let (delta_tx, mut delta_rx) =
        mpsc::channel::<CommunityMembershipDelta>(8);

    let cas: Arc<
        tokio::sync::Mutex<std::collections::HashMap<harmony_content::cid::ContentId, Vec<u8>>>,
    > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let cas_for_servicer = Arc::clone(&cas);
    tokio::spawn(async move {
        use harmony_app::content_store::CasOp;
        while let Some(op) = cas_op_rx.recv().await {
            match op {
                CasOp::PutLocal { cid, blob, reply } => {
                    cas_for_servicer.lock().await.insert(cid, blob);
                    if let Some(reply) = reply {
                        let _ = reply.send(Ok(()));
                    }
                }
                CasOp::GetOrFetch { cid, timeout: _, reply } => {
                    let v = cas_for_servicer.lock().await.get(&cid).cloned();
                    let _ = reply.send(Ok(v));
                }
            }
        }
    });
    tokio::spawn(async move {
        while let Some(bytes) = a_out_rx.recv().await {
            let _ = b_in_tx.send(bytes).await;
        }
    });

    let community_id = SpaceId([1u8; 16]);
    let mk = MembershipKey::new([0x42; 32]);
    let identity_a = PrivateIdentity::from_seed(&[0xa1; 32]);
    let admin = OwnerAddr(identity_a.identity.address_hash);
    let identity_a_pub = identity_a.identity.to_public_bytes();

    let state_a = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let state_b = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let tracker_a = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));
    let tracker_b = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));

    let cs_a: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx.clone(),
        Duration::from_millis(2000),
    ));
    let cs_b: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        Duration::from_millis(2000),
    ));

    {
        let mut sa = state_a.lock().await;
        let payload = EventPayload {
            id: [9u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: admin,
            at: Hlc { wall_ms: 100, logical: 0, device_id: "a-dev".into() },
        };
        let event = sign_event_with_identity(&payload, &identity_a).expect("sign");
        let _ = sa.insert_event(
            event,
            &harmony_app::community_membership::VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &identity_a_pub,
                countersigner_identity_pub: None,
            },
        );
    }

    let tmp_a = tempfile::tempdir().expect("tempdir a");
    let tmp_b = tempfile::tempdir().expect("tempdir b");

    let engine_a = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk.clone(),
        admin_addr: admin,
        is_invite_only: false,
        device_id: "a-dev".into(),
        state: Arc::clone(&state_a),
        tracker: Arc::clone(&tracker_a),
        content_store: cs_a,
        publisher_tx: a_out_tx,
        subscriber_rx: a_in_rx,
        paths: harmony_app::community_state_sync::PersistPaths {
            crdt: tmp_a.path().join("crdt.cbor"),
            replay: tmp_a.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: None,
        error_tx: None,
        delta_tx: None,
    });

    struct SingleIdentityResolver {
        addr: OwnerAddr,
        identity_pub: [u8; 64],
    }
    #[async_trait::async_trait]
    impl harmony_app::community_state_sync::IdentityResolver for SingleIdentityResolver {
        async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
            if *addr == self.addr { Some(self.identity_pub) } else { None }
        }
    }
    let resolver: Arc<dyn harmony_app::community_state_sync::IdentityResolver> =
        Arc::new(SingleIdentityResolver { addr: admin, identity_pub: identity_a_pub });

    let engine_b = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk,
        admin_addr: admin,
        is_invite_only: false,
        device_id: "b-dev".into(),
        state: Arc::clone(&state_b),
        tracker: Arc::clone(&tracker_b),
        content_store: cs_b,
        publisher_tx: b_out_tx,
        subscriber_rx: b_in_rx,
        paths: harmony_app::community_state_sync::PersistPaths {
            crdt: tmp_b.path().join("crdt.cbor"),
            replay: tmp_b.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(resolver),
        error_tx: None,
        delta_tx: Some(delta_tx),
    });

    engine_a.flush_now().await.expect("flush_now");

    let delta = tokio::time::timeout(Duration::from_secs(2), delta_rx.recv())
        .await
        .expect("delta should arrive within 2s")
        .expect("delta channel should be open");
    assert_eq!(delta.community_id, community_id);
    assert_eq!(delta.event.actor, admin);
    assert!(matches!(delta.event.kind, MembershipEventKind::Join));

    engine_a.shutdown().await.expect("shutdown a");
    engine_b.shutdown().await.expect("shutdown b");
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_sync_engine_unit engine_emits_membership_delta_on_remote_insert 2>&1 | tail -20
```

Expected: compile error referencing `delta_tx` not being a field of `CommunitySyncEngineConfig` and/or `CommunityMembershipDelta` not existing.

- [ ] **Step 3: Add `CommunityMembershipDelta` + extend config**

In `src-tauri/src/community_state_sync.rs`, after the `CommunityDegradedReport` struct (~line 374), add:

```rust
/// Membership-CRDT mutation surfaced from the engine to the IPC layer.
/// Fired on every `InsertOutcome::Inserted` — covers both the engine's
/// receive pipeline (DAG-synced events from peers) AND IPC-driven local
/// inserts via `CommunitySyncEngine::insert_local_event`.
///
/// Shipped as a flat `event` clone rather than a delta-typed payload
/// because the consumer (Phase 3's start_node delta task) needs the
/// event's `kind`, `actor`, `at`, and (for Kick) `reason` to build the
/// `community-members-changed` Tauri event payload — and shipping the
/// signed event is cheap (a few hundred bytes) and avoids duplicating
/// the per-kind switch inside the engine.
#[derive(Debug, Clone)]
pub struct CommunityMembershipDelta {
    pub community_id: SpaceId,
    pub event: SignedMembershipEvent,
}
```

Add the `use` import at the top of the file if `SignedMembershipEvent` isn't already imported (it should be; verify).

In `CommunitySyncEngineConfig`, add the new field at the end:

```rust
    /// Optional sink for membership CRDT mutations. Best-effort
    /// `try_send`; a closed or full channel surfaces as a dropped delta
    /// (the IPC consumer is purely informational, so back-pressuring
    /// the engine on a stuck consumer is wrong).
    pub delta_tx: Option<mpsc::Sender<CommunityMembershipDelta>>,
```

In `InternalCtx` (find the struct ~line 580), add the matching field. In `CommunitySyncEngine::new` (~line 443), pass `delta_tx: cfg.delta_tx` into the `internal_task`.

- [ ] **Step 4: Emit delta on `Inserted` in receive path**

Find `handle_incoming_publish` in `community_state_sync.rs` (~line 1014). Locate the loop that calls `state.insert_event(...)` and inspects the `InsertOutcome`. After an `Inserted` outcome, BEFORE acquiring any further locks, emit:

```rust
            if let Some(tx) = ctx.delta_tx.as_ref() {
                let _ = tx.try_send(CommunityMembershipDelta {
                    community_id: ctx.community_id,
                    event: event_clone.clone(),
                });
            }
```

Where `event_clone` is the inserted event (the existing code already has it cloned before insert; reuse the same binding). Place this emit OUTSIDE the state lock — `try_send` is non-blocking and the spec's "no `.await` while holding state mutex" rule still applies (technically `try_send` doesn't await, but keeping it lock-free preserves the pattern's clarity).

- [ ] **Step 5: Run test to verify it passes**

```bash
cd src-tauri
cargo fmt --all
set -o pipefail
cargo test --test community_sync_engine_unit engine_emits_membership_delta_on_remote_insert 2>&1 | tail -20
```

Expected: PASS.

- [ ] **Step 6: Run all gates**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
set -o pipefail
cargo test --all-targets 2>&1 | tail -5
```

Expected: all green; total test count = baseline + 1.

- [ ] **Step 7: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/community_state_sync.rs src-tauri/tests/community_sync_engine_unit.rs
git commit -m "feat(zeb-217-phase3): CommunityMembershipDelta + engine delta_tx emission on receive"
```

---

## Task 2: `CommunitySyncEngine::insert_local_event`

**Files:**

- Modify: `src-tauri/src/community_state_sync.rs` — add `pub async fn insert_local_event` on `impl CommunitySyncEngine` (~line 436), plus a small `LocalInsertError` enum next to `CommunitySyncError`.
- Modify: `src-tauri/tests/community_sync_engine_unit.rs` — add the new test from Step 1.

**Why a method on the engine, not free-standing logic in IPC handlers:**
The engine already centralizes the verify+insert+delta-emit semantics for the receive path. Local inserts must follow the same shape so a future audit can prove "every Prolly Tree insert went through one code path." Without a single entry point, IPC handlers would each grow a copy of the verify-then-insert-then-emit dance and the delta-emission rule would inevitably drift (someone forgets to fire on a new IPC). Centralizing also lets us keep `IdentityResolver` access private to the engine — IPC handlers don't need to know about Sub-A's owner-device cache.

- [ ] **Step 1: Write the failing test**

Append to `src-tauri/tests/community_sync_engine_unit.rs`:

```rust
#[tokio::test]
async fn engine_insert_local_event_emits_delta_and_notifies_publish() {
    use harmony_app::community_state_sync::{
        CommunityMembershipDelta, LocalInsertError,
    };
    use std::time::Duration;

    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(8);
    let (_in_tx, in_rx) = mpsc::channel::<Vec<u8>>(8);
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel(8);
    let (delta_tx, mut delta_rx) =
        mpsc::channel::<CommunityMembershipDelta>(8);

    tokio::spawn(async move {
        use harmony_app::content_store::CasOp;
        while let Some(op) = cas_op_rx.recv().await {
            if let CasOp::PutLocal { reply: Some(reply), .. } = op {
                let _ = reply.send(Ok(()));
            }
        }
    });

    let community_id = SpaceId([2u8; 16]);
    let mk = MembershipKey::new([0x33; 32]);
    let identity = PrivateIdentity::from_seed(&[0xc1; 32]);
    let admin = OwnerAddr(identity.identity.address_hash);
    let identity_pub = identity.identity.to_public_bytes();

    let state = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let tracker = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        Duration::from_millis(1000),
    ));
    let tmp = tempfile::tempdir().expect("tempdir");

    struct StaticResolver { addr: OwnerAddr, identity_pub: [u8; 64] }
    #[async_trait::async_trait]
    impl harmony_app::community_state_sync::IdentityResolver for StaticResolver {
        async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
            if *addr == self.addr { Some(self.identity_pub) } else { None }
        }
    }
    let resolver: Arc<dyn harmony_app::community_state_sync::IdentityResolver> =
        Arc::new(StaticResolver { addr: admin, identity_pub });

    let engine = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk,
        admin_addr: admin,
        is_invite_only: false,
        device_id: "local-dev".into(),
        state: Arc::clone(&state),
        tracker: Arc::clone(&tracker),
        content_store: cs,
        publisher_tx: out_tx,
        subscriber_rx: in_rx,
        paths: harmony_app::community_state_sync::PersistPaths {
            crdt: tmp.path().join("crdt.cbor"),
            replay: tmp.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(resolver),
        error_tx: None,
        delta_tx: Some(delta_tx),
    });

    let payload = EventPayload {
        id: [7u8; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin,
        at: Hlc { wall_ms: 1000, logical: 0, device_id: "local-dev".into() },
    };
    let event = sign_event_with_identity(&payload, &identity).expect("sign");

    let outcome = engine
        .insert_local_event(event.clone())
        .await
        .expect("insert_local_event should succeed");
    assert_eq!(
        outcome,
        harmony_app::community_state_crdt::InsertOutcome::Inserted
    );

    let delta = tokio::time::timeout(Duration::from_secs(1), delta_rx.recv())
        .await
        .expect("delta within 1s")
        .expect("delta channel open");
    assert_eq!(delta.event.id, event.id);

    let _bytes = tokio::time::timeout(Duration::from_secs(2), out_rx.recv())
        .await
        .expect("publish within 2s")
        .expect("publisher channel open");

    let outcome2 = engine.insert_local_event(event).await.expect("idempotent");
    assert_eq!(
        outcome2,
        harmony_app::community_state_crdt::InsertOutcome::AlreadyKnown
    );
    let none_delta =
        tokio::time::timeout(Duration::from_millis(200), delta_rx.recv()).await;
    assert!(none_delta.is_err(), "AlreadyKnown must not emit a delta");

    let bad_payload = EventPayload {
        id: [8u8; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: OwnerAddr([0xff; 16]),
        at: Hlc { wall_ms: 2000, logical: 0, device_id: "local-dev".into() },
    };
    let bad_event = sign_event_with_identity(&bad_payload, &identity).expect("sign");
    let result = engine.insert_local_event(bad_event).await;
    assert!(matches!(result, Err(LocalInsertError::Verify(_))
        | Ok(harmony_app::community_state_crdt::InsertOutcome::Rejected(_))));

    engine.shutdown().await.expect("shutdown");
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_sync_engine_unit engine_insert_local_event_emits_delta_and_notifies_publish 2>&1 | tail -10
```

Expected: compile error — `insert_local_event` and/or `LocalInsertError` undefined.

- [ ] **Step 3: Add `LocalInsertError` + the method**

In `src-tauri/src/community_state_sync.rs`, after `CommunitySyncError` (~line 264), add:

```rust
/// Failure modes specific to `CommunitySyncEngine::insert_local_event`.
/// Distinct enum (not a variant on `CommunitySyncError`) because local-
/// insert failures are caller-driven (bad event from IPC) rather than
/// transport / crypto class — the IPC layer needs to surface them as
/// distinct error strings to the frontend.
#[derive(thiserror::Error, Debug)]
pub enum LocalInsertError {
    #[error("identity_resolver not configured — engine cannot verify local events")]
    MissingIdentityResolver,
    #[error("actor identity not in resolver: {0:?}")]
    UnknownActor(OwnerAddr),
    #[error("verify_event rejected the local event: {0}")]
    Verify(crate::community_membership::VerifyError),
}
```

Then on `impl CommunitySyncEngine` (~line 436), add this method:

```rust
    /// Insert a locally-minted event into the community CRDT, verify it
    /// using the engine's `identity_resolver`, fire the membership-delta
    /// channel on `Inserted`, and notify the publish loop so the change
    /// reaches peers.
    ///
    /// Centralises the local-mint path so every IPC that mutates this
    /// community's CRDT (`create_community`, `redeem_invite`,
    /// `leave_community`, and Phase 4's kick / set_power / invite-only
    /// redeem) shares a single verify-then-insert-then-emit-delta path.
    /// Without this method, each IPC would grow a copy of the dance and
    /// the delta-emission rule would inevitably drift on a new variant.
    ///
    /// `Ok(InsertOutcome::Inserted)` — event landed; delta fired; publish
    /// notified. `Ok(InsertOutcome::AlreadyKnown)` — duplicate; no delta,
    /// no publish-notify (the previous insert already did both).
    /// `Ok(InsertOutcome::Rejected(VerifyError))` — verify failed at the
    /// CRDT layer (banned-stickiness etc.). `Err(LocalInsertError::*)`
    /// — failure BEFORE we got far enough to call insert (no resolver,
    /// or resolver couldn't find the actor).
    pub async fn insert_local_event(
        &self,
        event: crate::community_membership::SignedMembershipEvent,
    ) -> Result<crate::community_state_crdt::InsertOutcome, LocalInsertError> {
        let resolver = self
            .identity_resolver
            .as_ref()
            .ok_or(LocalInsertError::MissingIdentityResolver)?;

        let actor_pub = resolver
            .resolve(&event.actor)
            .await
            .ok_or(LocalInsertError::UnknownActor(event.actor))?;

        let countersigner_pub = if let Some(cs) = event.countersig.as_ref() {
            resolver.resolve(&cs.signer).await
        } else {
            None
        };

        let ctx = crate::community_membership::VerifyContext {
            expected_community_id: event.community_id,
            admin_addr: self.admin_addr,
            is_invite_only: self.is_invite_only,
            actor_identity_pub: &actor_pub,
            countersigner_identity_pub: countersigner_pub.as_ref(),
        };

        let outcome = {
            let mut state_g = self.state.lock().await;
            state_g.insert_event(event.clone(), &ctx)
        };

        if matches!(outcome, crate::community_state_crdt::InsertOutcome::Inserted) {
            if let Some(tx) = self.delta_tx.as_ref() {
                let _ = tx.try_send(CommunityMembershipDelta {
                    community_id: event.community_id,
                    event,
                });
            }
            self.notify_dirty();
        }

        Ok(outcome)
    }
```

This requires:

(a) Storing `identity_resolver`, `is_invite_only`, and `delta_tx` on `CommunitySyncEngine` (the struct ~line 410). Currently only `state`, `admin_addr`, etc. are retained. Add:

```rust
    identity_resolver: Option<Arc<dyn IdentityResolver>>,
    is_invite_only: bool,
    delta_tx: Option<mpsc::Sender<CommunityMembershipDelta>>,
```

(b) Initializing them in `CommunitySyncEngine::new` (~line 443) BEFORE moving `cfg` into `InternalCtx`:

```rust
        let identity_resolver_for_engine = cfg.identity_resolver.clone();
        let is_invite_only_for_engine = cfg.is_invite_only;
        let delta_tx_for_engine = cfg.delta_tx.clone();
```

And populating them in the `Self { ... }` constructor at the bottom.

- [ ] **Step 4: Run test to verify it passes**

```bash
cd src-tauri
cargo fmt --all
set -o pipefail
cargo test --test community_sync_engine_unit engine_insert_local_event_emits_delta_and_notifies_publish 2>&1 | tail -10
```

Expected: PASS.

- [ ] **Step 5: Run all gates**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
set -o pipefail
cargo test --all-targets 2>&1 | tail -5
```

Expected: all green; total test count = previous + 1.

- [ ] **Step 6: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/community_state_sync.rs src-tauri/tests/community_sync_engine_unit.rs
git commit -m "feat(zeb-217-phase3): CommunitySyncEngine::insert_local_event"
```

---

## Task 3: Add `base64` dependency + invite URL encode/decode helpers

**Files:**

- Modify: `src-tauri/Cargo.toml` — add `base64 = "0.22"` to `[dependencies]`.
- Modify: `src-tauri/src/community_invite.rs` — add `encode_invite_url`, `decode_invite_url`, and `InviteUrlError`.
- Modify: `src-tauri/tests/community_invite_unit.rs` — add round-trip + bad-input tests.

- [ ] **Step 1: Write the failing tests**

Append to `src-tauri/tests/community_invite_unit.rs`:

```rust
#[test]
fn invite_url_round_trips_open_payload() {
    use harmony_app::community_invite::{
        decode_invite_url, encode_invite_url, CommunityInvitePayload,
    };
    use harmony_app::owner_state_types::{MembershipKey, OwnerAddr, SpaceId};

    let payload = CommunityInvitePayload {
        community_id: SpaceId([0xab; 16]),
        membership_key: MembershipKey::new([0x42; 32]),
        admin_addr: OwnerAddr([0xcd; 16]),
        community_name: "Hackers United".to_string(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
    };

    let url = encode_invite_url(&payload).expect("encode");
    assert!(url.starts_with("harmony://invite/"));
    assert!(!url.contains('+') && !url.contains('/') && !url.contains('='),
        "base64url no-pad must not contain +, /, or =");

    let decoded = decode_invite_url(&url).expect("decode");
    assert_eq!(decoded, payload);
}

#[test]
fn decode_rejects_wrong_scheme() {
    use harmony_app::community_invite::{decode_invite_url, InviteUrlError};
    let err = decode_invite_url("https://example.com/invite/abc").unwrap_err();
    assert!(matches!(err, InviteUrlError::WrongScheme(_)));
}

#[test]
fn decode_rejects_invalid_base64() {
    use harmony_app::community_invite::{decode_invite_url, InviteUrlError};
    let err = decode_invite_url("harmony://invite/!!!not-base64!!!").unwrap_err();
    assert!(matches!(err, InviteUrlError::Base64(_)));
}

#[test]
fn decode_rejects_truncated_cbor() {
    use harmony_app::community_invite::{decode_invite_url, InviteUrlError};
    use base64::Engine;
    let truncated = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([0xa1, 0x62]);
    let url = format!("harmony://invite/{truncated}");
    let err = decode_invite_url(&url).unwrap_err();
    assert!(matches!(err, InviteUrlError::Cbor(_)));
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_invite_unit 2>&1 | tail -20
```

Expected: compile errors — `encode_invite_url`, `decode_invite_url`, `InviteUrlError` undefined; `base64` import unresolved.

- [ ] **Step 3: Add base64 dependency**

In `src-tauri/Cargo.toml`, under `[dependencies]`, add (alphabetised by convention):

```toml
base64 = "0.22"
```

Verify the addition:

```bash
cd src-tauri
cargo build 2>&1 | tail -3
```

Expected: clean build (no warnings about unused dep — the next step uses it).

- [ ] **Step 4: Implement helpers**

Append to `src-tauri/src/community_invite.rs`:

```rust
use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
use base64::Engine;

const URL_PREFIX: &str = "harmony://invite/";

/// Errors decoding a `harmony://invite/...` URL into a
/// `CommunityInvitePayload`. Distinct variants per failure class so the
/// IPC layer can surface a precise diagnostic to the frontend (and a
/// future telemetry dashboard can tally each independently).
#[derive(thiserror::Error, Debug)]
pub enum InviteUrlError {
    /// URL didn't start with `harmony://invite/`. Carries the leading
    /// chunk we saw so the operator can spot mistyped schemes (https://
    /// instead of harmony://, etc.).
    #[error("invite URL scheme must be `harmony://invite/`, got `{0}`")]
    WrongScheme(String),
    /// The base64url body failed to decode — typically a copy-paste
    /// truncation or a stray character that shouldn't be in URL-safe
    /// base64.
    #[error("base64url decode failed: {0}")]
    Base64(String),
    /// The decoded bytes weren't valid canonical CBOR for
    /// `CommunityInvitePayload`. Usually a truncated copy-paste, a
    /// version mismatch, or a non-invite payload encoded with the same
    /// scheme by mistake.
    #[error("CBOR decode failed: {0}")]
    Cbor(String),
}

/// Canonical-CBOR-encode the payload, then base64url-no-pad the result,
/// and prefix `harmony://invite/`. The output is copy-paste-safe across
/// chat / email / messaging clients that munge `+`, `/`, or `=`.
pub fn encode_invite_url(payload: &CommunityInvitePayload) -> Result<String, InviteUrlError> {
    let cbor = canonical_cbor_encode(payload).map_err(|e| InviteUrlError::Cbor(e.to_string()))?;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&cbor);
    Ok(format!("{URL_PREFIX}{b64}"))
}

/// Strip the `harmony://invite/` prefix, base64url-decode, then
/// canonical-CBOR-decode into a `CommunityInvitePayload`. All three
/// failure classes are distinguished in `InviteUrlError`.
pub fn decode_invite_url(url: &str) -> Result<CommunityInvitePayload, InviteUrlError> {
    let body = url.strip_prefix(URL_PREFIX).ok_or_else(|| {
        InviteUrlError::WrongScheme(url.chars().take(URL_PREFIX.len()).collect())
    })?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body)
        .map_err(|e| InviteUrlError::Base64(e.to_string()))?;
    canonical_cbor_decode::<CommunityInvitePayload>(&bytes)
        .map_err(|e| InviteUrlError::Cbor(e.to_string()))
}
```

- [ ] **Step 5: Run tests to verify they pass**

```bash
cd src-tauri
cargo fmt --all
set -o pipefail
cargo test --test community_invite_unit 2>&1 | tail -10
```

Expected: all 4 new tests PASS plus the existing tests still pass.

- [ ] **Step 6: Run all gates**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
set -o pipefail
cargo test --all-targets 2>&1 | tail -5
```

Expected: all green.

- [ ] **Step 7: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/community_invite.rs src-tauri/tests/community_invite_unit.rs
git commit -m "feat(zeb-217-phase3): invite URL encode/decode + base64url helpers"
```

---

## Task 4: `MemberInfoDto` + sorted projection helper

**Files:**

- Modify: `src-tauri/src/lib.rs` — add `MemberInfoDto` (Tauri-exposed type) and a free function `member_info_for(materialized: &MaterializedMembership) -> Vec<MemberInfoDto>` that sorts by power desc then joined_at asc.
- Modify: `src-tauri/src/lib.rs` — add a unit test in the existing `#[cfg(test)] mod tests` block (or create a new one) for the sort helper.

**Why a free function not a method on `MaterializedMembership`:** the DTO carries hex-encoded strings (frontend convenience) and lives in `lib.rs` where Tauri serialization is wired. Putting it on `MaterializedMembership` would force `community_membership.rs` to depend on hex / Tauri shapes — wrong direction.

- [ ] **Step 1: Write the failing test**

In `src-tauri/src/lib.rs`, find the existing `#[cfg(test)] mod tests` block (search `mod tests`) or append one at the bottom of the file. Add:

```rust
#[cfg(test)]
mod community_member_dto_tests {
    use super::{member_info_for, MemberInfoDto};
    use crate::community_membership::{MaterializedMembership, MemberState, MemberStatus};
    use crate::owner_state_types::{Hlc, OwnerAddr};
    use std::collections::BTreeMap;

    fn hlc(wall: u64, dev: &str) -> Hlc {
        Hlc { wall_ms: wall, logical: 0, device_id: dev.to_string() }
    }

    #[test]
    fn member_info_sorts_by_power_desc_then_joined_at_asc() {
        let admin = OwnerAddr([1; 16]);
        let mod_user = OwnerAddr([2; 16]);
        let early = OwnerAddr([3; 16]);
        let late = OwnerAddr([4; 16]);

        let mut members = BTreeMap::new();
        members.insert(admin, MemberState {
            status: MemberStatus::Joined, joined_at: hlc(100, "a"), left_at: None,
        });
        members.insert(mod_user, MemberState {
            status: MemberStatus::Joined, joined_at: hlc(200, "b"), left_at: None,
        });
        members.insert(early, MemberState {
            status: MemberStatus::Joined, joined_at: hlc(150, "c"), left_at: None,
        });
        members.insert(late, MemberState {
            status: MemberStatus::Joined, joined_at: hlc(300, "d"), left_at: None,
        });

        let mut power_levels = BTreeMap::new();
        power_levels.insert(admin, 100);
        power_levels.insert(mod_user, 50);

        let materialized = MaterializedMembership { members, power_levels };
        let dto = member_info_for(&materialized);

        assert_eq!(dto.len(), 4);
        assert_eq!(dto[0].addr, hex::encode(admin.0));
        assert_eq!(dto[0].power, 100);
        assert_eq!(dto[1].addr, hex::encode(mod_user.0));
        assert_eq!(dto[1].power, 50);
        assert_eq!(dto[2].addr, hex::encode(early.0));
        assert_eq!(dto[2].power, 0);
        assert_eq!(dto[3].addr, hex::encode(late.0));
        assert_eq!(dto[3].power, 0);
    }

    #[test]
    fn member_info_includes_left_and_banned_members() {
        let a = OwnerAddr([1; 16]);
        let b = OwnerAddr([2; 16]);
        let mut members = BTreeMap::new();
        members.insert(a, MemberState {
            status: MemberStatus::Left, joined_at: hlc(100, "x"),
            left_at: Some(hlc(200, "x")),
        });
        members.insert(b, MemberState {
            status: MemberStatus::Banned, joined_at: hlc(50, "y"),
            left_at: Some(hlc(150, "y")),
        });
        let materialized = MaterializedMembership {
            members, power_levels: BTreeMap::new(),
        };
        let dto = member_info_for(&materialized);
        assert_eq!(dto.len(), 2);
        let statuses: Vec<_> = dto.iter().map(|d| d.status).collect();
        assert!(statuses.contains(&MemberStatus::Left));
        assert!(statuses.contains(&MemberStatus::Banned));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd src-tauri
set -o pipefail
cargo test community_member_dto_tests 2>&1 | tail -10
```

Expected: compile error — `MemberInfoDto`, `member_info_for` undefined.

- [ ] **Step 3: Implement DTO + helper**

Add near the other community-related types in `src-tauri/src/lib.rs` (search for an existing `Serialize`-derived community type or add a new `// ── ZEB-217 community IPC types ──` section). Insert:

```rust
/// Member-list row returned by `list_community_members` IPC. Mirrors
/// the spec's MemberInfo type. `addr` is hex of OwnerAddr (16 bytes →
/// 32 chars). `display_name` is None in Phase 3 — the existing profile
/// cache lookup is wired in Phase 5.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberInfoDto {
    pub addr: String,
    pub display_name: Option<String>,
    pub status: crate::community_membership::MemberStatus,
    pub power: u8,
    pub joined_at: crate::owner_state_types::Hlc,
}

/// Project a materialized membership into the IPC DTO list, sorted by
/// power level descending then joined_at ascending. Stable for two
/// addrs at the same power+joined_at — falls through to OwnerAddr-bytes
/// comparison so the order is deterministic across calls.
pub fn member_info_for(
    m: &crate::community_membership::MaterializedMembership,
) -> Vec<MemberInfoDto> {
    let mut rows: Vec<MemberInfoDto> = m
        .members
        .iter()
        .map(|(addr, state)| MemberInfoDto {
            addr: hex::encode(addr.0),
            display_name: None,
            status: state.status,
            power: m.power_levels.get(addr).copied().unwrap_or(0),
            joined_at: state.joined_at.clone(),
        })
        .collect();
    rows.sort_by(|a, b| {
        b.power
            .cmp(&a.power)
            .then_with(|| a.joined_at.wall_ms.cmp(&b.joined_at.wall_ms))
            .then_with(|| a.joined_at.logical.cmp(&b.joined_at.logical))
            .then_with(|| a.addr.cmp(&b.addr))
    });
    rows
}
```

- [ ] **Step 4: Run test to verify it passes**

```bash
cd src-tauri
cargo fmt --all
set -o pipefail
cargo test community_member_dto_tests 2>&1 | tail -10
```

Expected: both new tests PASS.

- [ ] **Step 5: Run all gates**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
set -o pipefail
cargo test --all-targets 2>&1 | tail -5
```

Expected: all green.

- [ ] **Step 6: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-217-phase3): MemberInfoDto + sorted projection helper"
```

---

## Task 5: `list_community_members` IPC (read-only)

**Files:**

- Modify: `src-tauri/src/lib.rs` — add `#[tauri::command] async fn list_community_members(...)` and register it in the `tauri::generate_handler!` macro at the bottom of the `run()` builder.

**Why a separate task from the inner DTO/helper:** the IPC layer adds two responsibilities the helper doesn't have — locating the community engine via `community_registry`, and translating "no engine for this id" / "registry not running" / "bad hex" into `Err(String)`. Keeping these concerns separate from the pure projection makes both easier to test.

- [ ] **Step 1: Write the failing test**

Append to the end of `src-tauri/src/lib.rs` (or in a new test module):

```rust
#[cfg(test)]
mod list_community_members_ipc_tests {
    use super::*;
    use crate::community_membership::{MembershipEventKind, sign_event_with_identity, EventPayload};
    use crate::community_state_crdt::CommunityState;
    use crate::community_state_sync::*;
    use crate::owner_state_types::*;
    use harmony_identity::PrivateIdentity;
    use std::sync::Arc;
    use tokio::sync::{mpsc, Mutex};

    #[tokio::test]
    async fn list_members_returns_sorted_dto_for_known_community() {
        let community_id = SpaceId([5; 16]);
        let mk = MembershipKey::new([0x55; 32]);
        let identity = PrivateIdentity::from_seed(&[0xab; 32]);
        let admin = OwnerAddr(identity.identity.address_hash);
        let identity_pub = identity.identity.to_public_bytes();

        let state = Arc::new(Mutex::new(CommunityState::new(community_id)));
        {
            let mut sa = state.lock().await;
            let payload = EventPayload {
                id: [1; 16], community_id, kind: MembershipEventKind::Join,
                actor: admin,
                at: Hlc { wall_ms: 100, logical: 0, device_id: "x".into() },
            };
            let evt = sign_event_with_identity(&payload, &identity).expect("sign");
            let _ = sa.insert_event(evt, &crate::community_membership::VerifyContext {
                expected_community_id: community_id,
                admin_addr: admin,
                is_invite_only: false,
                actor_identity_pub: &identity_pub,
                countersigner_identity_pub: None,
            });
        }

        let materialized = state.lock().await.materialize_now(admin);
        let dto = member_info_for(&materialized);
        assert_eq!(dto.len(), 1);
        assert_eq!(dto[0].addr, hex::encode(admin.0));
        assert_eq!(dto[0].power, 100);
    }
}
```

This test directly exercises `member_info_for` against a state populated through `insert_event` rather than the IPC tauri::State plumbing — testing the IPC command itself end-to-end requires the Tauri test harness which Phase 5 sets up via ZEB-247. The IPC wrapper is mostly plumbing; correctness is in `member_info_for` (Task 4) and `materialize_now` (Phase 1, already covered).

- [ ] **Step 2: Run test to verify it fails**

```bash
cd src-tauri
set -o pipefail
cargo test list_community_members_ipc_tests 2>&1 | tail -10
```

Expected: PASS (the test only uses already-existing helpers — what we add in Step 3 is the IPC wrapper, which the test doesn't directly call). If you see a fail at this point, compare your test to the snippet above. Step 3 still adds the IPC wrapper for actual IPC invocation by the frontend.

- [ ] **Step 3: Implement the IPC wrapper**

In `src-tauri/src/lib.rs`, near the other `#[tauri::command]` functions, add:

```rust
/// Read-only IPC over a community's materialized member list.
/// Returns rows sorted by power desc then joined_at asc (see
/// `member_info_for`). `community_id_hex` is the 32-char lowercase
/// hex of the 16-byte SpaceId.
///
/// Errors:
/// - `Err("invalid community_id hex: ...")` — couldn't parse hex.
/// - `Err("no community_registry — node not running?")` — start_node
///   hasn't wired the registry.
/// - `Err("no engine for community {hex} — not joined or not yet
///   started")` — the community isn't in the registry's map.
#[tauri::command]
async fn list_community_members(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
) -> Result<Vec<MemberInfoDto>, String> {
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let registry = {
        let g = state_lock.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        g.community_registry
            .clone()
            .ok_or("no community_registry — node not running?")?
    };

    let engine_state = registry
        .state_for(&space_id)
        .await
        .ok_or_else(|| {
            format!(
                "no engine for community {} — not joined or not yet started",
                hex::encode(space_id.0)
            )
        })?;

    let admin_addr = {
        let engines = registry.known_ids().await;
        if !engines.iter().any(|id| *id == space_id) {
            return Err(format!(
                "engine vanished for community {} between state_for and known_ids",
                hex::encode(space_id.0)
            ));
        }
        let g = engine_state.lock().await;
        let log: Vec<crate::community_membership::SignedMembershipEvent> =
            g.events.values().cloned().collect();
        if let Some(first) = log.first() {
            first.actor
        } else {
            return Ok(Vec::new());
        }
    };

    let materialized = {
        let g = engine_state.lock().await;
        g.materialize_now(admin_addr)
    };

    Ok(member_info_for(&materialized))
}
```

**Note on the `admin_addr` derivation:** Phase 3 doesn't yet plumb the per-community `admin_addr` from the owner-state Space row through to the IPC layer (`registry.state_for` only hands back the CRDT). For `list_community_members` we approximate by reading the bootstrap `Join` event's `actor` (which IS the admin in v1, since open communities only have one creator and `redeem_invite` doesn't change `admin_addr`). Task 9 will plumb the real `admin_addr` via a registry accessor; until then the bootstrap-Join shortcut is correct for open-only Phase 3.

If the engine has zero events (hasn't received its own bootstrap insert yet because the frontend called `list_community_members` faster than the local insert + flush completed), return an empty Vec rather than `Err` — this is the natural empty-state shape and the frontend can show "No members yet."

Then add `list_community_members` to the `tauri::generate_handler!` macro near the bottom of the `run()` function. Search for `tauri::generate_handler!` and append `list_community_members,` to the comma-separated list.

- [ ] **Step 4: Run test to verify it passes**

```bash
cd src-tauri
cargo fmt --all
set -o pipefail
cargo test list_community_members_ipc_tests 2>&1 | tail -10
```

Expected: PASS.

- [ ] **Step 5: Run all gates**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
set -o pipefail
cargo test --all-targets 2>&1 | tail -5
```

Expected: all green.

- [ ] **Step 6: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-217-phase3): list_community_members IPC"
```

---

## Task 6: `generate_invite` IPC (open path — token-less)

**Files:**

- Modify: `src-tauri/src/lib.rs` — add `#[tauri::command] async fn generate_invite(...)`. Register in `generate_handler!`.

**Phase 3 scope on `generate_invite`:** open communities only. The `invitee_hint` and `expires_at` parameters are accepted in the signature (matching the spec's IPC contract) but Phase 3 only emits a token-less payload — `invite_token = None`. Phase 4 will add inviter-key signing and produce the InviteToken signature. The IPC validates the caller is a current member (any joined member can invite per `POWER_THRESHOLDS.invite = 0`).

- [ ] **Step 1: Write the failing test**

Append to `src-tauri/src/lib.rs`:

```rust
#[cfg(test)]
mod generate_invite_helper_tests {
    use super::*;
    use crate::community_invite::{decode_invite_url, CommunityInvitePayload};
    use crate::owner_state_types::{MembershipKey, OwnerAddr, SpaceId};

    #[test]
    fn build_open_invite_payload_round_trips_via_url() {
        let payload = CommunityInvitePayload {
            community_id: SpaceId([7; 16]),
            membership_key: MembershipKey::new([0x99; 32]),
            admin_addr: OwnerAddr([0x11; 16]),
            community_name: "DoorClub".into(),
            is_invite_only: false,
            expires_at: None,
            invite_token: None,
        };
        let url = build_open_invite_url(&payload).expect("url");
        let decoded = decode_invite_url(&url).expect("decode");
        assert_eq!(decoded, payload);
        assert!(decoded.invite_token.is_none(), "open path must be token-less");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd src-tauri
set -o pipefail
cargo test generate_invite_helper_tests 2>&1 | tail -10
```

Expected: compile error — `build_open_invite_url` undefined.

- [ ] **Step 3: Implement the helper + IPC wrapper**

In `src-tauri/src/lib.rs`, add the helper:

```rust
/// Encode a CommunityInvitePayload into the harmony://invite/ URL form.
/// Thin wrapper over `community_invite::encode_invite_url` so call sites
/// don't need to import the lower-level error type — surfaces failures
/// as `Result<String, String>` matching the IPC convention.
pub fn build_open_invite_url(
    payload: &crate::community_invite::CommunityInvitePayload,
) -> Result<String, String> {
    crate::community_invite::encode_invite_url(payload)
        .map_err(|e| format!("encode invite URL: {e}"))
}
```

Then add the `#[tauri::command]`:

```rust
/// Generate a `harmony://invite/...` URL for an OPEN community. The
/// returned URL carries the community id + symmetric `MembershipKey` +
/// admin addr + community name, so any holder can decrypt the
/// state-root topic and publish their own Join event.
///
/// `invitee_hint` and `expires_at` are accepted to match the spec's IPC
/// contract but are unused in Phase 3 — Phase 4 will sign an
/// `InviteToken` carrying both. Phase 3 returns a token-less payload.
///
/// Errors:
/// - `Err("invalid community_id hex: ...")` — bad hex.
/// - `Err("no community_registry — node not running?")` — registry not
///   wired (start_node hasn't run).
/// - `Err("no Space for community {hex} in owner-state")` — the
///   community isn't in our local owner-state (we haven't joined or
///   we left).
/// - `Err("community Space missing membership_key / admin_addr / kind")`
///   — defensive guard; should be unreachable since
///   `validate_invariants` rejects these on apply, but cheap to check.
#[tauri::command]
async fn generate_invite(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    invitee_hint: Option<String>,
    expires_at: Option<u64>,
) -> Result<String, String> {
    let _ = (invitee_hint, expires_at); // Phase 4 wiring; ignored in Phase 3.

    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let crdt_state = {
        let g = state_lock.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        g.crdt_state.clone().ok_or("crdt_state missing — node not running?")?
    };

    let space = {
        let s = crdt_state.lock().await;
        s.spaces.get(&space_id).cloned()
    }
    .ok_or_else(|| format!("no Space for community {} in owner-state", hex::encode(space_id.0)))?;

    if space.kind != crate::owner_state_types::SpaceKind::Community {
        return Err(format!(
            "Space {} exists but is kind {:?}, not Community",
            hex::encode(space_id.0),
            space.kind
        ));
    }
    let mk = space
        .membership_key
        .clone()
        .ok_or("community Space missing membership_key (corrupt row?)")?;
    let admin = space
        .admin_addr
        .ok_or("community Space missing admin_addr (corrupt row?)")?;
    let is_invite_only = space.is_invite_only.unwrap_or(false);

    if is_invite_only {
        return Err(
            "Phase 3 supports OPEN communities only; invite-only generate_invite ships in Phase 4"
                .to_string(),
        );
    }

    let payload = crate::community_invite::CommunityInvitePayload {
        community_id: space_id,
        membership_key: mk,
        admin_addr: admin,
        community_name: space.name.clone(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
    };
    build_open_invite_url(&payload)
}
```

Add `generate_invite` to `tauri::generate_handler!`.

- [ ] **Step 4: Run test to verify it passes**

```bash
cd src-tauri
cargo fmt --all
set -o pipefail
cargo test generate_invite_helper_tests 2>&1 | tail -10
```

Expected: PASS.

- [ ] **Step 5: Run all gates**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
set -o pipefail
cargo test --all-targets 2>&1 | tail -5
```

Expected: all green.

- [ ] **Step 6: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-217-phase3): generate_invite IPC (open path)"
```

---

## Task 7: `community-members-changed` payload types + delta-to-event projection

**Files:**

- Modify: `src-tauri/src/lib.rs` — add `CommunityMembersChangedPayload`, `MembershipChange` enum, and a free function `delta_to_change(delta: &CommunityMembershipDelta) -> Option<(String, MembershipChange)>` that returns `(community_id_hex, change)` or `None` if the event kind is unrepresentable (forward-compat slot).

**Why this task is separate from start_node wiring (Task 8):** the projection logic is pure and testable without spinning up a runtime, plus the types are needed by Task 8's consumer task. Splitting keeps the test surface for the projection visible.

- [ ] **Step 1: Write the failing test**

In `src-tauri/src/lib.rs`, add a test module:

```rust
#[cfg(test)]
mod community_delta_projection_tests {
    use super::*;
    use crate::community_membership::{
        sign_event_with_identity, EventPayload, MembershipEventKind,
    };
    use crate::community_state_sync::CommunityMembershipDelta;
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;

    fn make_delta(kind: MembershipEventKind, actor: OwnerAddr) -> CommunityMembershipDelta {
        let identity = PrivateIdentity::from_seed(&[0xee; 32]);
        let community_id = SpaceId([4; 16]);
        let payload = EventPayload {
            id: [0xab; 16], community_id, kind, actor,
            at: Hlc { wall_ms: 1234, logical: 0, device_id: "x".into() },
        };
        let event = sign_event_with_identity(&payload, &identity).expect("sign");
        CommunityMembershipDelta { community_id, event }
    }

    #[test]
    fn join_projects_to_joined_change() {
        let actor = OwnerAddr([1; 16]);
        let (cid_hex, change) = delta_to_change(&make_delta(MembershipEventKind::Join, actor))
            .expect("Join projects");
        assert_eq!(cid_hex, hex::encode([4u8; 16]));
        match change {
            MembershipChange::Joined { addr, at_wall_ms } => {
                assert_eq!(addr, hex::encode(actor.0));
                assert_eq!(at_wall_ms, 1234);
            }
            other => panic!("expected Joined, got {other:?}"),
        }
    }

    #[test]
    fn leave_projects_to_left_change() {
        let actor = OwnerAddr([2; 16]);
        let (_, change) = delta_to_change(&make_delta(MembershipEventKind::Leave, actor)).unwrap();
        assert!(matches!(change, MembershipChange::Left { addr, .. } if addr == hex::encode(actor.0)));
    }

    #[test]
    fn kick_projects_with_target_and_actor_as_by() {
        let actor = OwnerAddr([3; 16]);
        let target = OwnerAddr([4; 16]);
        let (_, change) = delta_to_change(&make_delta(
            MembershipEventKind::Kick { target, reason: Some("spam".into()) },
            actor,
        )).unwrap();
        match change {
            MembershipChange::Kicked { addr, by, reason, .. } => {
                assert_eq!(addr, hex::encode(target.0));
                assert_eq!(by, hex::encode(actor.0));
                assert_eq!(reason.as_deref(), Some("spam"));
            }
            other => panic!("expected Kicked, got {other:?}"),
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd src-tauri
set -o pipefail
cargo test community_delta_projection_tests 2>&1 | tail -10
```

Expected: compile error — `delta_to_change`, `MembershipChange`, `CommunityMembersChangedPayload` undefined.

- [ ] **Step 3: Implement types + projection**

Add to `src-tauri/src/lib.rs`:

```rust
/// Delta-style payload for the `community-members-changed` Tauri event.
/// One emit per CRDT mutation — own appends AND DAG-synced peer events.
/// Frontend updates incrementally without re-fetching the full member
/// list. Phase 3 fires only `Joined` / `Left`; Phase 4 fires
/// `Invited` / `Kicked` / `PowerChanged`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommunityMembersChangedPayload {
    pub community_id: String,
    pub change: MembershipChange,
}

/// Per-event projection. `addr` is the entity whose membership state
/// changed; `by` (when present) is the actor who caused the change.
/// `at_wall_ms` is the event's HLC wall-ms — frontend uses it to sort
/// or de-dupe rapid-fire deltas.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum MembershipChange {
    Joined { addr: String, at_wall_ms: u64 },
    Left { addr: String, at_wall_ms: u64 },
    Invited { addr: String, by: String, at_wall_ms: u64 },
    Kicked { addr: String, by: String, reason: Option<String>, at_wall_ms: u64 },
    PowerChanged { addr: String, by: String, level: u8, at_wall_ms: u64 },
}

/// Project a `CommunityMembershipDelta` into the IPC change tuple. The
/// caller (the start_node consumer task) wraps this in
/// `CommunityMembersChangedPayload` and emits the Tauri event.
///
/// Returns `None` for kinds we can't yet represent (none today; reserved
/// for forward-compat if the membership enum grows).
pub fn delta_to_change(
    delta: &crate::community_state_sync::CommunityMembershipDelta,
) -> Option<(String, MembershipChange)> {
    let cid_hex = hex::encode(delta.community_id.0);
    let by = hex::encode(delta.event.actor.0);
    let at_wall_ms = delta.event.at.wall_ms;
    let change = match &delta.event.kind {
        crate::community_membership::MembershipEventKind::Join => {
            MembershipChange::Joined { addr: by.clone(), at_wall_ms }
        }
        crate::community_membership::MembershipEventKind::Leave => {
            MembershipChange::Left { addr: by.clone(), at_wall_ms }
        }
        crate::community_membership::MembershipEventKind::Invite { target } => {
            MembershipChange::Invited {
                addr: hex::encode(target.0), by, at_wall_ms,
            }
        }
        crate::community_membership::MembershipEventKind::Kick { target, reason } => {
            MembershipChange::Kicked {
                addr: hex::encode(target.0), by, reason: reason.clone(), at_wall_ms,
            }
        }
        crate::community_membership::MembershipEventKind::SetPower { target, level } => {
            MembershipChange::PowerChanged {
                addr: hex::encode(target.0), by, level: *level, at_wall_ms,
            }
        }
    };
    Some((cid_hex, change))
}
```

- [ ] **Step 4: Run test to verify it passes**

```bash
cd src-tauri
cargo fmt --all
set -o pipefail
cargo test community_delta_projection_tests 2>&1 | tail -10
```

Expected: 3 PASS.

- [ ] **Step 5: Run all gates**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
set -o pipefail
cargo test --all-targets 2>&1 | tail -5
```

Expected: all green.

- [ ] **Step 6: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-217-phase3): community-members-changed payload + delta projection"
```

---

## Task 8: Wire delta channel + degraded channel in `start_node`

**Files:**

- Modify: `src-tauri/src/lib.rs` — extend `NodeState` with `community_delta_tx: Option<mpsc::Sender<CommunityMembershipDelta>>`; in `start_node`, create the `(delta_tx, delta_rx)` channel + the `(degraded_tx, degraded_rx)` channel BEFORE constructing the `CommunityRegistryConfig`; pass them into the registry config; spawn one consumer task per channel that owns the `AppHandle` and emits Tauri events; reset both fields on `stop_node`.

**Why both channels here:** Phase 2 already shipped `error_tx` (CommunityDegradedReport) plumbing through the engine but no consumer task — the channel was created in tests only. Phase 3 wires both end-to-end so the spec's IPC events fire.

- [ ] **Step 1: Write the failing test**

This task is integration-shaped (start_node wiring). Pure unit tests for the consumer task itself are useful — write one in `src-tauri/src/lib.rs`:

```rust
#[cfg(test)]
mod delta_consumer_task_tests {
    use super::*;
    use crate::community_membership::{
        sign_event_with_identity, EventPayload, MembershipEventKind,
    };
    use crate::community_state_sync::CommunityMembershipDelta;
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;

    #[tokio::test]
    async fn consumer_emits_payload_via_handler() {
        let (tx, rx) = tokio::sync::mpsc::channel::<CommunityMembershipDelta>(8);
        let captured: std::sync::Arc<tokio::sync::Mutex<Vec<CommunityMembersChangedPayload>>> =
            std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let captured_for_handler = std::sync::Arc::clone(&captured);

        let handle = tokio::spawn(async move {
            run_community_delta_consumer(rx, move |payload| {
                let captured = std::sync::Arc::clone(&captured_for_handler);
                async move { captured.lock().await.push(payload); }
            }).await
        });

        let identity = PrivateIdentity::from_seed(&[0xab; 32]);
        let actor = OwnerAddr(identity.identity.address_hash);
        let community_id = SpaceId([6; 16]);
        let payload = EventPayload {
            id: [9; 16], community_id, kind: MembershipEventKind::Join,
            actor, at: Hlc { wall_ms: 100, logical: 0, device_id: "x".into() },
        };
        let event = sign_event_with_identity(&payload, &identity).unwrap();
        tx.send(CommunityMembershipDelta { community_id, event }).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let cap = captured.lock().await;
        assert_eq!(cap.len(), 1);
        assert_eq!(cap[0].community_id, hex::encode(community_id.0));
        assert!(matches!(cap[0].change, MembershipChange::Joined { .. }));
        drop(tx);
        let _ = handle.await;
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd src-tauri
set -o pipefail
cargo test delta_consumer_task_tests 2>&1 | tail -10
```

Expected: compile error — `run_community_delta_consumer` undefined.

- [ ] **Step 3: Implement the consumer task helper**

Add to `src-tauri/src/lib.rs`:

```rust
/// Drain `delta_rx`, project each delta into `CommunityMembersChangedPayload`,
/// and pass to `emit`. Stops cleanly when the channel closes (last sender
/// dropped — typically on `stop_node`). Pure async fn; the start_node
/// caller wraps the closure with `app.emit(...)` and spawns this task.
///
/// Generic over `Emit` so the unit test can pass a Vec-pushing closure
/// instead of needing a real `AppHandle`. Production call site uses
/// `move |p| async move { let _ = app.emit("community-members-changed", &p); }`.
pub async fn run_community_delta_consumer<F, Fut>(
    mut delta_rx: tokio::sync::mpsc::Receiver<crate::community_state_sync::CommunityMembershipDelta>,
    mut emit: F,
) where
    F: FnMut(CommunityMembersChangedPayload) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    while let Some(delta) = delta_rx.recv().await {
        if let Some((community_id, change)) = delta_to_change(&delta) {
            let payload = CommunityMembersChangedPayload { community_id, change };
            emit(payload).await;
        }
    }
}

/// Mirror for `CommunityDegradedReport`. Emits the spec's
/// `community-state-sync-degraded` Tauri event with `{ communityId,
/// reason }`. Same lifecycle as the delta consumer.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommunityStateSyncDegradedPayload {
    pub community_id: String,
    pub reason: String,
}

pub async fn run_community_degraded_consumer<F, Fut>(
    mut degraded_rx: tokio::sync::mpsc::Receiver<crate::community_state_sync::CommunityDegradedReport>,
    mut emit: F,
) where
    F: FnMut(CommunityStateSyncDegradedPayload) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    while let Some(report) = degraded_rx.recv().await {
        let payload = CommunityStateSyncDegradedPayload {
            community_id: hex::encode(report.community_id.0),
            reason: report.reason_tag.to_string(),
        };
        emit(payload).await;
    }
}
```

**Cross-check the field name** on `CommunityDegradedReport` — Phase 2 ships it with `reason_tag: &'static str` plus a `detail: Option<String>`. Use whichever fields exist; the test in Step 1 only exercises the delta consumer. If `CommunityDegradedReport` has `tag` instead of `reason_tag`, adjust the field access.

- [ ] **Step 4: Wire into NodeState + start_node**

In `NodeState` (search for `pub struct NodeState`), add:

```rust
    /// Sender side of the community delta channel — IPC handlers can
    /// (Phase 4+) reach into this directly for fan-out beyond the engine
    /// path; Phase 3 keeps the engine as the single producer.
    pub community_delta_tx: Option<tokio::sync::mpsc::Sender<crate::community_state_sync::CommunityMembershipDelta>>,
```

In the `Default` / construction site of `NodeState`, initialise to `None`. In `stop_node`'s reset path (look for `community_registry.take()`), `take()` this too so a stopped node doesn't dangle.

In `start_node`, in the block where `CommunityRegistryConfig` is built (search `CommunityRegistryConfig`), create channels BEFORE building the config:

```rust
        let (community_delta_tx, community_delta_rx) =
            tokio::sync::mpsc::channel::<crate::community_state_sync::CommunityMembershipDelta>(256);
        let (community_degraded_tx, community_degraded_rx) =
            tokio::sync::mpsc::channel::<crate::community_state_sync::CommunityDegradedReport>(256);
```

Pass them into the registry config (and into each `CommunitySyncEngineConfig`'s `delta_tx` field via `cfg.delta_tx.clone()`):

```rust
        let registry_cfg = CommunityRegistryConfig {
            // ... existing fields ...
            error_tx: Some(community_degraded_tx),
            delta_tx: Some(community_delta_tx.clone()),
        };
```

(If `CommunityRegistryConfig` doesn't yet plumb `delta_tx` through to spawned engines, extend it: add the field to the struct, pass it into each `spawn_engine`'s `CommunitySyncEngineConfig`. This is a small change in `community_state_sync.rs` — should land in this same task since it's the wiring counterpart of Task 1.)

After `state.lock()` completes and `community_registry_arc` is set, spawn the two consumer tasks:

```rust
        let app_for_delta = app.clone();
        tokio::spawn(run_community_delta_consumer(
            community_delta_rx,
            move |payload| {
                let app = app_for_delta.clone();
                async move {
                    let _ = app.emit("community-members-changed", &payload);
                }
            },
        ));
        let app_for_degraded = app.clone();
        tokio::spawn(run_community_degraded_consumer(
            community_degraded_rx,
            move |payload| {
                let app = app_for_degraded.clone();
                async move {
                    let _ = app.emit("community-state-sync-degraded", &payload);
                }
            },
        ));
```

Store `community_delta_tx` on `NodeState` in the same block where `community_registry` is stored.

- [ ] **Step 5: Run test to verify it passes**

```bash
cd src-tauri
cargo fmt --all
set -o pipefail
cargo test delta_consumer_task_tests 2>&1 | tail -10
```

Expected: PASS.

- [ ] **Step 6: Run all gates**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
set -o pipefail
cargo test --all-targets 2>&1 | tail -5
```

Expected: all green.

- [ ] **Step 7: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/lib.rs src-tauri/src/community_state_sync.rs
git commit -m "feat(zeb-217-phase3): wire community delta + degraded consumers in start_node"
```

---

## Task 9: `create_community` IPC

**Files:**

- Modify: `src-tauri/src/lib.rs` — add `#[tauri::command] async fn create_community(...)`. Register in `generate_handler!`. Add a small inner pure function `mint_community_creation(...)` so the IPC's snapshot-then-spawn fence is testable separately.

**Flow (per spec §"Architecture / Phase 3 — Open community flow"):**

1. Generate fresh `community_id: SpaceId` (random 16 bytes) and `membership_key: MembershipKey` (random 32 bytes).
2. Mint the bootstrap-admin self-Join `SignedMembershipEvent { actor: self, kind: Join, ... }`.
3. Build the Community `Space` row and `apply_space_with_canonicalization` it onto owner-state.
4. Snapshot owner-state's HLC tracker advancement (mirrors `add_space`).
5. **Snapshot-then-spawn fence** — re-acquire NodeState lock; if `generation` changed or `community_registry` is `None`, return `Err` (engine can't be wired). The Space is in a detached `crdt_state` and won't be persisted, but we suppress the engine spawn so we don't leak a phantom-state engine.
6. Create `(pub_tx, pub_rx)` and `(sub_tx, sub_rx)` mpsc pairs; call `registry.spawn_engine(community_id, mk, admin, false, pub_tx, sub_rx)`. Push a `CommunityAdapterRequest` through some new path so the event_loop wires the channels to Zenoh — see "Adapter wiring" below.
7. Call `engine.insert_local_event(join_event)` so the bootstrap Join lands in the CRDT, the delta channel fires, and `notify_dirty` triggers the first publish.
8. Return `community_id_hex`.

**Adapter wiring (the tricky part):** `start_node` currently spawns Zenoh adapters at boot, BEFORE the runtime is fully up, by walking the existing community Spaces. For NEW communities created mid-session, we need a path to spawn an adapter on-the-fly. Two options:

- (a) Add an `mpsc::Sender<CommunityAdapterRequest>` from `start_node` into `event_loop`, and a select arm in event_loop that calls `spawn_community_state_zenoh_adapter` per request. Mirrors how `unicast_send_tx` shuttles requests in.
- (b) Have `create_community` directly spawn the adapter task itself (it has access to `Arc<Session>`).

Option (a) is the right shape — keeps `event_loop` as the owner of all Zenoh-touching tasks and lets the closing flag flow through cleanly. **This task implements option (a):**

- Add `community_adapter_request_tx: Option<mpsc::Sender<event_loop::CommunityAdapterRequest>>` to `NodeState`, populated by `start_node`.
- In `event_loop` (the existing `select!` loop), add an arm that drains the request channel and calls `spawn_community_state_zenoh_adapter` for each.

- [ ] **Step 1: Write the failing test**

In `src-tauri/src/lib.rs`:

```rust
#[cfg(test)]
mod create_community_inner_tests {
    use super::*;
    use crate::owner_state_types::{Hlc, OwnerAddr};
    use harmony_identity::PrivateIdentity;

    #[test]
    fn mint_creation_produces_consistent_id_join_event_and_space() {
        let identity = PrivateIdentity::from_seed(&[0xc1; 32]);
        let identity_pub = identity.identity.to_public_bytes();
        let self_owner = OwnerAddr(identity.identity.address_hash);
        let device_id = "creator-dev";
        let prev_hlc: Option<Hlc> = None;
        let wall_now_ms = 1_700_000_000_000u64;

        let minted = mint_community_creation(
            "Hackers United",
            false,
            self_owner,
            &identity,
            &identity_pub,
            device_id,
            wall_now_ms,
            prev_hlc.as_ref(),
        ).expect("mint");

        assert_eq!(minted.space.kind, crate::owner_state_types::SpaceKind::Community);
        assert_eq!(minted.space.id, minted.community_id);
        assert_eq!(minted.space.admin_addr, Some(self_owner));
        assert_eq!(minted.space.is_invite_only, Some(false));
        assert!(minted.space.membership_key.is_some());

        assert_eq!(minted.bootstrap_join.actor, self_owner);
        assert_eq!(minted.bootstrap_join.community_id, minted.community_id);
        assert!(matches!(
            minted.bootstrap_join.kind,
            crate::community_membership::MembershipEventKind::Join
        ));
        assert_eq!(minted.bootstrap_join.at.wall_ms, wall_now_ms);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd src-tauri
set -o pipefail
cargo test create_community_inner_tests 2>&1 | tail -10
```

Expected: compile error — `mint_community_creation`, `MintedCommunity` undefined.

- [ ] **Step 3: Implement the inner pure function**

Add to `src-tauri/src/lib.rs`:

```rust
/// Pure, side-effect-free output of `mint_community_creation`. The IPC
/// wrapper applies these to owner-state, the registry, and the engine
/// in sequence — keeping the minting pure makes `create_community`'s
/// fence logic testable independent of NodeState plumbing.
pub struct MintedCommunity {
    pub community_id: crate::owner_state_types::SpaceId,
    pub membership_key: crate::owner_state_types::MembershipKey,
    pub space: crate::owner_state_types::Space,
    pub bootstrap_join: crate::community_membership::SignedMembershipEvent,
}

/// Generate a fresh community id + membership key, build the bootstrap-
/// admin self-Join event signed under `identity`, and assemble the
/// matching Community Space row. Pure (no I/O). The IPC wrapper handles
/// owner-state apply + registry spawn + engine.insert_local_event.
pub fn mint_community_creation(
    name: &str,
    is_invite_only: bool,
    self_owner: crate::owner_state_types::OwnerAddr,
    identity: &harmony_identity::PrivateIdentity,
    _identity_pub: &[u8; 64],
    device_id: &str,
    wall_now_ms: u64,
    prev_hlc: Option<&crate::owner_state_types::Hlc>,
) -> Result<MintedCommunity, String> {
    use crate::community_membership::{sign_event_with_identity, EventPayload, MembershipEventKind};
    use crate::owner_state_types::{Hlc, MembershipKey, Space, SpaceId, SpaceKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut id_bytes = [0u8; 16];
    rng.fill_bytes(&mut id_bytes);
    let community_id = SpaceId(id_bytes);

    let mut mk_bytes = [0u8; 32];
    rng.fill_bytes(&mut mk_bytes);
    let membership_key = MembershipKey::new(mk_bytes);

    let creation_hlc = crate::dm_outbox::next_hlc(prev_hlc, wall_now_ms, device_id);

    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);
    let join_payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::Join,
        actor: self_owner,
        at: creation_hlc.clone(),
    };
    let bootstrap_join = sign_event_with_identity(&join_payload, identity)
        .map_err(|e| format!("sign bootstrap join: {e}"))?;

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

    Ok(MintedCommunity { community_id, membership_key, space, bootstrap_join })
}
```

Confirm `rand` is in `Cargo.toml` `[dependencies]` (the codebase uses it elsewhere). If absent, add `rand = "0.8"` and update Cargo.lock.

- [ ] **Step 4: Implement the IPC wrapper**

```rust
#[tauri::command]
async fn create_community(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    name: String,
    is_invite_only: bool,
) -> Result<String, String> {
    if is_invite_only {
        return Err(
            "Phase 3 supports OPEN communities only; invite-only create_community ships in Phase 4"
                .to_string(),
        );
    }

    let (
        crdt_state,
        hlc_tracker,
        device_id,
        self_owner,
        identity_pub_64,
        community_registry,
        community_adapter_tx,
        identity,
        snapshot_generation,
    ) = {
        let g = state_lock.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state.clone().ok_or("crdt_state missing — node not running?")?,
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.dm_identity_pub_64.ok_or("dm_identity_pub_64 missing")?,
            g.community_registry.clone().ok_or("community_registry missing")?,
            g.community_adapter_request_tx
                .clone()
                .ok_or("community_adapter_request_tx missing")?,
            g.private_identity.clone().ok_or("private_identity missing")?,
            g.generation,
        )
    };

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let minted = {
        let prev_hlc = {
            let t = hlc_tracker.lock().await;
            t.get(&device_id).cloned()
        };
        mint_community_creation(
            &name,
            is_invite_only,
            self_owner,
            &identity,
            &identity_pub_64,
            &device_id,
            wall_now_ms,
            prev_hlc.as_ref(),
        )?
    };

    {
        let mut state_g = crdt_state.lock().await;
        let outcome = state_g.apply_space_with_canonicalization(minted.space.clone());
        if matches!(outcome, crate::owner_state_crdt::ApplyOutcome::Rejected(_)) {
            return Err(format!("apply_space rejected new community: {outcome:?}"));
        }
        let mut tracker_g = hlc_tracker.lock().await;
        tracker_g.insert(device_id.clone(), minted.space.created_at.clone());
    }

    {
        let g = state_lock.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            return Err(format!(
                "node generation changed during create_community (was {}, now {})",
                snapshot_generation, g.generation
            ));
        }
        if g.community_registry.is_none() {
            return Err(
                "community_registry was torn down during create_community — engine spawn suppressed"
                    .to_string(),
            );
        }
    }

    let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

    community_registry
        .spawn_engine(
            minted.community_id,
            minted.membership_key.clone(),
            self_owner,
            is_invite_only,
            pub_tx,
            sub_rx,
        )
        .await
        .map_err(|e| format!("registry.spawn_engine: {e}"))?;

    community_adapter_tx
        .send(crate::event_loop::CommunityAdapterRequest {
            id_hex: hex::encode(minted.community_id.0),
            publisher_rx: pub_rx,
            subscriber_tx: sub_tx,
        })
        .await
        .map_err(|e| format!("community_adapter_tx send: {e}"))?;

    let engine_state = community_registry
        .state_for(&minted.community_id)
        .await
        .ok_or("engine vanished immediately after spawn — registry race")?;
    let _ = engine_state;

    let engine_arc = community_registry
        .engine_arc(&minted.community_id)
        .await
        .ok_or("engine vanished immediately after spawn (engine_arc lookup)")?;
    let outcome = engine_arc
        .insert_local_event(minted.bootstrap_join)
        .await
        .map_err(|e| format!("engine.insert_local_event: {e}"))?;
    if !matches!(outcome, crate::community_state_crdt::InsertOutcome::Inserted) {
        return Err(format!("bootstrap Join not inserted (got {outcome:?})"));
    }

    Ok(hex::encode(minted.community_id.0))
}
```

This requires extending `CommunitySyncRegistry` with a public `engine_arc(community_id) -> Option<Arc<CommunitySyncEngine>>` accessor since the existing `state_for` only hands back the state. Add it next to `state_for` (~line 1583 in `community_state_sync.rs`):

```rust
    /// Returns the `Arc<CommunitySyncEngine>` for `community_id`, if a
    /// engine is spawned. Used by Phase 3 IPCs that need to call
    /// `engine.insert_local_event(...)`. Mirrors `state_for` shape but
    /// returns the engine handle instead of just the state.
    pub async fn engine_arc(&self, community_id: &SpaceId) -> Option<Arc<CommunitySyncEngine>> {
        self.engines.lock().await.get(community_id).cloned()
    }
```

Add `community_adapter_request_tx: Option<mpsc::Sender<event_loop::CommunityAdapterRequest>>` to `NodeState`. In `start_node`, after the boot-time community scan loops (where `community_adapter_requests: Vec<CommunityAdapterRequest>` is built), create an mpsc channel for ongoing requests:

```rust
        let (community_adapter_request_tx, community_adapter_request_rx) =
            tokio::sync::mpsc::channel::<crate::event_loop::CommunityAdapterRequest>(32);
```

Pass `community_adapter_request_rx` into `event_loop::run_event_loop` (or whatever the entry function is) so a select arm can drain it. Pass `community_adapter_request_tx` into NodeState.

In `event_loop.rs`, find the main `select!` and add an arm:

```rust
                Some(req) = community_adapter_request_rx.recv() => {
                    spawn_community_state_zenoh_adapter(
                        Arc::clone(&session_arc),
                        req.id_hex,
                        req.publisher_rx,
                        req.subscriber_tx,
                        Arc::clone(&closing),
                    );
                }
```

(`session_arc` may need to be created earlier in the function since it's currently created inside the boot scan only when there are pre-existing communities. Move the `let session_arc = Arc::new(session.clone());` to before the `select!` so it's always available.)

Add `create_community` to `tauri::generate_handler!`.

- [ ] **Step 5: Run test to verify it passes**

```bash
cd src-tauri
cargo fmt --all
set -o pipefail
cargo test create_community_inner_tests 2>&1 | tail -10
```

Expected: PASS.

- [ ] **Step 6: Run all gates**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
set -o pipefail
cargo test --all-targets 2>&1 | tail -5
```

Expected: all green.

- [ ] **Step 7: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/lib.rs src-tauri/src/community_state_sync.rs src-tauri/src/event_loop.rs
git commit -m "feat(zeb-217-phase3): create_community IPC + on-demand adapter wiring"
```

---

## Task 10: `redeem_invite` IPC (open path only)

**Files:**

- Modify: `src-tauri/src/lib.rs` — add `#[tauri::command] async fn redeem_invite(url: String)`. Register in `generate_handler!`. Add an inner pure function `mint_redemption(...)` returning the `(Space, bootstrap-Join)` so the snapshot-then-spawn fence is testable.

**Flow:**

1. `decode_invite_url(url)` → `CommunityInvitePayload`.
2. If `payload.is_invite_only` → return `Err` (Phase 4).
3. Snapshot NodeState handles (mirroring `create_community`).
4. Mint the joiner's self-Join event signed under `identity`, with `actor = self_owner`, `community_id = payload.community_id`.
5. Build the Space row from `payload.community_id` + `payload.membership_key` + `payload.admin_addr` + `payload.community_name` + `is_invite_only: false`. Note: the Space `id` IS the `payload.community_id` — both peers' Space rows must share the same id so dedupe collapses correctly.
6. `apply_space_with_canonicalization`. The same-SpaceId rejection of community-creation field changes from Phase 1's `apply_space` defends against malicious or stale invites that contradict an existing local Space.
7. Snapshot-then-spawn fence (same as `create_community`).
8. `registry.spawn_engine(...)` — registry's spawn is idempotent, so re-redeeming the same invite (e.g., user clicked twice) is a no-op.
9. `event_loop`-side adapter request via `community_adapter_request_tx`.
10. `engine.insert_local_event(self_join)` — fires delta + notifies publish.
11. Return `community_id_hex`.

- [ ] **Step 1: Write the failing test**

In `src-tauri/src/lib.rs`:

```rust
#[cfg(test)]
mod redeem_invite_inner_tests {
    use super::*;
    use crate::community_invite::CommunityInvitePayload;
    use crate::owner_state_types::{Hlc, MembershipKey, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;

    #[test]
    fn mint_redemption_produces_self_join_and_matching_space() {
        let identity = PrivateIdentity::from_seed(&[0xab; 32]);
        let identity_pub = identity.identity.to_public_bytes();
        let self_owner = OwnerAddr(identity.identity.address_hash);

        let payload = CommunityInvitePayload {
            community_id: SpaceId([0xee; 16]),
            membership_key: MembershipKey::new([0x77; 32]),
            admin_addr: OwnerAddr([0x33; 16]),
            community_name: "TestCom".into(),
            is_invite_only: false,
            expires_at: None,
            invite_token: None,
        };

        let device_id = "joiner-dev";
        let wall_now_ms = 1_700_000_999_000u64;
        let prev_hlc: Option<Hlc> = None;

        let minted = mint_redemption(
            &payload, self_owner, &identity, &identity_pub,
            device_id, wall_now_ms, prev_hlc.as_ref(),
        ).expect("mint");

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
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd src-tauri
set -o pipefail
cargo test redeem_invite_inner_tests 2>&1 | tail -10
```

Expected: compile error — `mint_redemption` undefined.

- [ ] **Step 3: Implement the inner function + IPC wrapper**

```rust
pub fn mint_redemption(
    payload: &crate::community_invite::CommunityInvitePayload,
    self_owner: crate::owner_state_types::OwnerAddr,
    identity: &harmony_identity::PrivateIdentity,
    _identity_pub: &[u8; 64],
    device_id: &str,
    wall_now_ms: u64,
    prev_hlc: Option<&crate::owner_state_types::Hlc>,
) -> Result<MintedCommunity, String> {
    use crate::community_membership::{sign_event_with_identity, EventPayload, MembershipEventKind};
    use crate::owner_state_types::{Space, SpaceKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let join_hlc = crate::dm_outbox::next_hlc(prev_hlc, wall_now_ms, device_id);

    let join_payload = EventPayload {
        id: event_id_bytes,
        community_id: payload.community_id,
        kind: MembershipEventKind::Join,
        actor: self_owner,
        at: join_hlc.clone(),
    };
    let bootstrap_join = sign_event_with_identity(&join_payload, identity)
        .map_err(|e| format!("sign self-join: {e}"))?;

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
        is_invite_only: Some(false),
    };

    Ok(MintedCommunity {
        community_id: payload.community_id,
        membership_key: payload.membership_key.clone(),
        space,
        bootstrap_join,
    })
}

#[tauri::command]
async fn redeem_invite(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    url: String,
) -> Result<String, String> {
    let payload = crate::community_invite::decode_invite_url(&url)
        .map_err(|e| format!("decode invite URL: {e}"))?;
    if payload.is_invite_only {
        return Err(
            "Phase 3 supports OPEN invite redemption only; invite-only ships in Phase 4"
                .to_string(),
        );
    }

    let (
        crdt_state, hlc_tracker, device_id, self_owner, identity_pub_64,
        community_registry, community_adapter_tx, identity, snapshot_generation,
    ) = {
        let g = state_lock.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state.clone().ok_or("crdt_state missing — node not running?")?,
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.dm_identity_pub_64.ok_or("dm_identity_pub_64 missing")?,
            g.community_registry.clone().ok_or("community_registry missing")?,
            g.community_adapter_request_tx
                .clone()
                .ok_or("community_adapter_request_tx missing")?,
            g.private_identity.clone().ok_or("private_identity missing")?,
            g.generation,
        )
    };

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let minted = {
        let prev_hlc = {
            let t = hlc_tracker.lock().await;
            t.get(&device_id).cloned()
        };
        mint_redemption(
            &payload, self_owner, &identity, &identity_pub_64,
            &device_id, wall_now_ms, prev_hlc.as_ref(),
        )?
    };

    {
        let mut state_g = crdt_state.lock().await;
        let outcome = state_g.apply_space_with_canonicalization(minted.space.clone());
        if matches!(outcome, crate::owner_state_crdt::ApplyOutcome::Rejected(_)) {
            return Err(format!("apply_space rejected redemption Space: {outcome:?}"));
        }
        let mut tracker_g = hlc_tracker.lock().await;
        tracker_g.insert(device_id.clone(), minted.space.created_at.clone());
    }

    {
        let g = state_lock.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            return Err(format!(
                "node generation changed during redeem_invite (was {}, now {})",
                snapshot_generation, g.generation
            ));
        }
        if g.community_registry.is_none() {
            return Err("community_registry torn down during redeem_invite".to_string());
        }
    }

    let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

    community_registry
        .spawn_engine(
            minted.community_id, minted.membership_key.clone(),
            payload.admin_addr, false, pub_tx, sub_rx,
        )
        .await
        .map_err(|e| format!("registry.spawn_engine: {e}"))?;

    community_adapter_tx
        .send(crate::event_loop::CommunityAdapterRequest {
            id_hex: hex::encode(minted.community_id.0),
            publisher_rx: pub_rx,
            subscriber_tx: sub_tx,
        })
        .await
        .map_err(|e| format!("community_adapter_tx send: {e}"))?;

    let engine_arc = community_registry
        .engine_arc(&minted.community_id)
        .await
        .ok_or("engine vanished immediately after spawn — registry race")?;
    engine_arc
        .insert_local_event(minted.bootstrap_join)
        .await
        .map_err(|e| format!("engine.insert_local_event: {e}"))?;

    Ok(hex::encode(minted.community_id.0))
}
```

Note `mint_redemption` returns `MintedCommunity` (defined in Task 9) — same shape as `mint_community_creation`. Reusing the type is intentional: both flows produce identical downstream wiring.

Add `redeem_invite` to `tauri::generate_handler!`.

- [ ] **Step 4: Run test to verify it passes**

```bash
cd src-tauri
cargo fmt --all
set -o pipefail
cargo test redeem_invite_inner_tests 2>&1 | tail -10
```

Expected: PASS.

- [ ] **Step 5: Run all gates**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
set -o pipefail
cargo test --all-targets 2>&1 | tail -5
```

Expected: all green.

- [ ] **Step 6: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-217-phase3): redeem_invite IPC (open path)"
```

---

## Task 11: `leave_community` IPC

**Files:**

- Modify: `src-tauri/src/lib.rs` — add `#[tauri::command] async fn leave_community(...)`. Register in `generate_handler!`.

**Flow:**

1. Parse `community_id` hex.
2. Snapshot NodeState handles.
3. Mint a `Leave` event signed under `identity` with `actor = self_owner`.
4. Look up the engine via `community_registry.engine_arc(&space_id)`. If not present → `Err` (you can't leave a community you're not in).
5. `engine.insert_local_event(leave_event)` — fires delta, triggers publish so peers learn we left.
6. Optionally bump the local Space's `left_at` field via owner-state mutation. Per spec §"IPC surface": `leave_community` does NOT remove the Space — caller must follow with `remove_space`. Phase 3 follows this pattern: only the CRDT Leave event + Space.left_at update; Space removal is a separate IPC. (The existing `remove_space` IPC handles Space removal generically.)

**Engine lifecycle on leave:** Phase 3 does NOT call `registry.stop_engine` from `leave_community`. Reason: the Leave event must publish to peers, and the engine's debounced publish loop owns that. Stopping the engine immediately after `insert_local_event` could race the publish. The user's eventual `remove_space` call (or a follow-up `forget_community` IPC, which is out of scope here) would call `registry.stop_engine`. For Phase 3 we leave the engine running so peers receive the Leave broadcast.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod leave_community_inner_tests {
    use super::*;
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;

    #[test]
    fn mint_leave_produces_self_leave_event() {
        let identity = PrivateIdentity::from_seed(&[0xab; 32]);
        let self_owner = OwnerAddr(identity.identity.address_hash);
        let community_id = SpaceId([0x77; 16]);
        let device_id = "leaver-dev";
        let prev_hlc: Option<Hlc> = None;
        let wall_now_ms = 1_700_000_500_000u64;

        let event = mint_leave_event(
            community_id, self_owner, &identity, device_id, wall_now_ms, prev_hlc.as_ref(),
        ).expect("mint");

        assert_eq!(event.actor, self_owner);
        assert_eq!(event.community_id, community_id);
        assert!(matches!(
            event.kind, crate::community_membership::MembershipEventKind::Leave
        ));
        assert_eq!(event.at.wall_ms, wall_now_ms);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd src-tauri
set -o pipefail
cargo test leave_community_inner_tests 2>&1 | tail -10
```

Expected: compile error — `mint_leave_event` undefined.

- [ ] **Step 3: Implement the inner function + IPC wrapper**

```rust
pub fn mint_leave_event(
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    identity: &harmony_identity::PrivateIdentity,
    device_id: &str,
    wall_now_ms: u64,
    prev_hlc: Option<&crate::owner_state_types::Hlc>,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{sign_event_with_identity, EventPayload, MembershipEventKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let leave_hlc = crate::dm_outbox::next_hlc(prev_hlc, wall_now_ms, device_id);
    let payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::Leave,
        actor: self_owner,
        at: leave_hlc,
    };
    sign_event_with_identity(&payload, identity).map_err(|e| format!("sign leave: {e}"))
}

#[tauri::command]
async fn leave_community(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
) -> Result<(), String> {
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let (
        hlc_tracker, device_id, self_owner, community_registry, identity, snapshot_generation,
    ) = {
        let g = state_lock.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry.clone().ok_or("community_registry missing")?,
            g.private_identity.clone().ok_or("private_identity missing")?,
            g.generation,
        )
    };

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let prev_hlc = {
        let t = hlc_tracker.lock().await;
        t.get(&device_id).cloned()
    };

    let leave = mint_leave_event(
        space_id, self_owner, &identity, &device_id, wall_now_ms, prev_hlc.as_ref(),
    )?;

    {
        let g = state_lock.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            return Err(format!(
                "node generation changed during leave_community (was {}, now {})",
                snapshot_generation, g.generation
            ));
        }
    }

    let engine_arc = community_registry
        .engine_arc(&space_id)
        .await
        .ok_or_else(|| {
            format!("no engine for community {} — not currently joined", hex::encode(space_id.0))
        })?;
    let outcome = engine_arc
        .insert_local_event(leave.clone())
        .await
        .map_err(|e| format!("engine.insert_local_event: {e}"))?;
    if matches!(outcome, crate::community_state_crdt::InsertOutcome::Rejected(_)) {
        return Err(format!("Leave rejected by CRDT verify: {outcome:?}"));
    }

    {
        let mut t = hlc_tracker.lock().await;
        t.insert(device_id.clone(), leave.at.clone());
    }

    Ok(())
}
```

Add `leave_community` to `tauri::generate_handler!`.

- [ ] **Step 4: Run test to verify it passes**

```bash
cd src-tauri
cargo fmt --all
set -o pipefail
cargo test leave_community_inner_tests 2>&1 | tail -10
```

Expected: PASS.

- [ ] **Step 5: Run all gates**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
set -o pipefail
cargo test --all-targets 2>&1 | tail -5
```

Expected: all green.

- [ ] **Step 6: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-217-phase3): leave_community IPC"
```

---

## Task 12: Integration test — full open community round-trip

**Files:**

- Create: `src-tauri/tests/community_open_flow_integration.rs` — exercises the inner pure functions (`mint_community_creation`, `mint_redemption`, `mint_leave_event`, `member_info_for`, `delta_to_change`) and the engine `insert_local_event` end-to-end across two engines bridged in-memory. Mirrors the shape of the existing `community_sync_integration.rs` (Phase 2's two-engine bridge) but exercises Phase 3's local-mint path on both sides.

**Why integration-shaped despite no Tauri harness:** the IPC `#[tauri::command]` wrappers can't be invoked without a real `AppHandle` until ZEB-247 lands in Phase 5. The pure-function inner helpers (`mint_*`, `member_info_for`, `delta_to_change`) cover the behavior the IPCs orchestrate; bridging them through two real `CommunitySyncEngine` instances catches the cross-peer convergence that unit tests miss.

- [ ] **Step 1: Create the integration test file**

Create `src-tauri/tests/community_open_flow_integration.rs`:

```rust
//! Two-engine open community round-trip — Phase 3 ZEB-217 Sub-C.
//!
//! Exercises the local-mint path (`mint_community_creation`,
//! `mint_redemption`, `mint_leave_event`) through two
//! `CommunitySyncEngine` instances bridged in-memory. Verifies:
//!
//! 1. Creator's bootstrap Join → published → received on B → materialized
//! 2. B's redemption-Join → published → received on A → materialized
//! 3. Both peers' member lists agree (sorted by power desc, joined_at asc)
//! 4. B's Leave → published → received on A → materialized as Left
//!
//! Cannot exercise the `#[tauri::command]` wrappers directly (no
//! AppHandle in tests until ZEB-247). The IPC wrappers are thin
//! plumbing over the inner pure helpers tested here.

use harmony_app::community_membership::{MaterializedMembership, MemberStatus};
use harmony_app::community_state_crdt::{CommunityState, InsertOutcome};
use harmony_app::community_state_sync::{
    CommunityMembershipDelta, CommunityRootHlcTracker, CommunitySyncEngine,
    CommunitySyncEngineConfig, IdentityResolver, PersistPaths, DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{CasOp, ContentStore, RuntimeContentStore};
use harmony_app::owner_state_types::{Hlc, MembershipKey, OwnerAddr, SpaceId};
use harmony_app::{
    delta_to_change, member_info_for, mint_community_creation, mint_leave_event,
    mint_redemption, MembershipChange,
};
use harmony_identity::PrivateIdentity;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

struct TwoIdentityResolver {
    a: (OwnerAddr, [u8; 64]),
    b: (OwnerAddr, [u8; 64]),
}

#[async_trait::async_trait]
impl IdentityResolver for TwoIdentityResolver {
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        if *addr == self.a.0 { Some(self.a.1) }
        else if *addr == self.b.0 { Some(self.b.1) }
        else { None }
    }
}

async fn wait_until<F, Fut>(mut cond: F, timeout: Duration) -> bool
where F: FnMut() -> Fut, Fut: std::future::Future<Output = bool> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if cond().await { return true; }
        if tokio::time::Instant::now() > deadline { return false; }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn open_community_create_redeem_leave_round_trip() {
    let identity_a = PrivateIdentity::from_seed(&[0xa1; 32]);
    let identity_b = PrivateIdentity::from_seed(&[0xb2; 32]);
    let owner_a = OwnerAddr(identity_a.identity.address_hash);
    let owner_b = OwnerAddr(identity_b.identity.address_hash);
    let pub_a = identity_a.identity.to_public_bytes();
    let pub_b = identity_b.identity.to_public_bytes();

    let resolver: Arc<dyn IdentityResolver> = Arc::new(TwoIdentityResolver {
        a: (owner_a, pub_a), b: (owner_b, pub_b),
    });

    let cas: Arc<Mutex<HashMap<harmony_content::cid::ContentId, Vec<u8>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (cas_op_tx, mut cas_op_rx) = mpsc::channel(64);
    let cas_for_servicer = Arc::clone(&cas);
    tokio::spawn(async move {
        while let Some(op) = cas_op_rx.recv().await {
            match op {
                CasOp::PutLocal { cid, blob, reply } => {
                    cas_for_servicer.lock().await.insert(cid, blob);
                    if let Some(r) = reply { let _ = r.send(Ok(())); }
                }
                CasOp::GetOrFetch { cid, timeout: _, reply } => {
                    let v = cas_for_servicer.lock().await.get(&cid).cloned();
                    let _ = reply.send(Ok(v));
                }
            }
        }
    });

    let (a_out_tx, mut a_out_rx) = mpsc::channel::<Vec<u8>>(64);
    let (a_in_tx, a_in_rx) = mpsc::channel::<Vec<u8>>(64);
    let (b_out_tx, mut b_out_rx) = mpsc::channel::<Vec<u8>>(64);
    let (b_in_tx, b_in_rx) = mpsc::channel::<Vec<u8>>(64);
    let a_in_for_fwd = a_in_tx.clone();
    tokio::spawn(async move {
        while let Some(bytes) = b_out_rx.recv().await {
            let _ = a_in_for_fwd.send(bytes).await;
        }
    });
    let b_in_for_fwd = b_in_tx.clone();
    tokio::spawn(async move {
        while let Some(bytes) = a_out_rx.recv().await {
            let _ = b_in_for_fwd.send(bytes).await;
        }
    });

    let minted_a = mint_community_creation(
        "TestCommunity", false, owner_a, &identity_a, &pub_a, "a-dev",
        100_000, None,
    ).expect("mint create");
    let community_id = minted_a.community_id;

    let cs_a: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx.clone(), Duration::from_secs(2),
    ));
    let cs_b: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx.clone(), Duration::from_secs(2),
    ));

    let state_a = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let state_b = Arc::new(Mutex::new(CommunityState::new(community_id)));
    let tracker_a = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));
    let tracker_b = Arc::new(Mutex::new(CommunityRootHlcTracker::default()));

    let (delta_a_tx, mut delta_a_rx) = mpsc::channel::<CommunityMembershipDelta>(32);
    let (delta_b_tx, mut delta_b_rx) = mpsc::channel::<CommunityMembershipDelta>(32);

    let tmp_a = tempfile::tempdir().expect("tmp a");
    let tmp_b = tempfile::tempdir().expect("tmp b");

    let engine_a = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id, membership_key: minted_a.membership_key.clone(),
        admin_addr: owner_a, is_invite_only: false, device_id: "a-dev".into(),
        state: Arc::clone(&state_a), tracker: Arc::clone(&tracker_a),
        content_store: cs_a, publisher_tx: a_out_tx, subscriber_rx: a_in_rx,
        paths: PersistPaths {
            crdt: tmp_a.path().join("crdt.cbor"),
            replay: tmp_a.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(Arc::clone(&resolver)),
        error_tx: None, delta_tx: Some(delta_a_tx),
    });
    let engine_b = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id, membership_key: minted_a.membership_key.clone(),
        admin_addr: owner_a, is_invite_only: false, device_id: "b-dev".into(),
        state: Arc::clone(&state_b), tracker: Arc::clone(&tracker_b),
        content_store: cs_b, publisher_tx: b_out_tx, subscriber_rx: b_in_rx,
        paths: PersistPaths {
            crdt: tmp_b.path().join("crdt.cbor"),
            replay: tmp_b.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(Arc::clone(&resolver)),
        error_tx: None, delta_tx: Some(delta_b_tx),
    });

    let outcome = engine_a
        .insert_local_event(minted_a.bootstrap_join.clone())
        .await
        .expect("A bootstrap insert");
    assert_eq!(outcome, InsertOutcome::Inserted);
    let delta_a_first = tokio::time::timeout(Duration::from_secs(1), delta_a_rx.recv())
        .await.expect("A own delta").expect("channel open");
    let (cid_hex_a, change_a) = delta_to_change(&delta_a_first).expect("project");
    assert_eq!(cid_hex_a, hex::encode(community_id.0));
    assert!(matches!(change_a, MembershipChange::Joined { .. }));

    assert!(wait_until(|| async {
        state_b.lock().await.events.len() == 1
    }, Duration::from_secs(3)).await, "B should receive A's bootstrap Join");
    let _delta_b_remote = tokio::time::timeout(Duration::from_secs(1), delta_b_rx.recv())
        .await.expect("B remote delta").expect("channel open");

    let invite_payload = harmony_app::community_invite::CommunityInvitePayload {
        community_id, membership_key: minted_a.membership_key.clone(),
        admin_addr: owner_a, community_name: "TestCommunity".into(),
        is_invite_only: false, expires_at: None, invite_token: None,
    };

    let minted_b = mint_redemption(
        &invite_payload, owner_b, &identity_b, &pub_b, "b-dev",
        200_000, None,
    ).expect("mint redeem");
    engine_b.insert_local_event(minted_b.bootstrap_join.clone())
        .await.expect("B redemption insert");
    let _delta_b_own = tokio::time::timeout(Duration::from_secs(1), delta_b_rx.recv())
        .await.expect("B own delta").expect("channel open");

    assert!(wait_until(|| async {
        state_a.lock().await.events.len() == 2
    }, Duration::from_secs(3)).await, "A should receive B's redemption Join");
    let _delta_a_remote = tokio::time::timeout(Duration::from_secs(1), delta_a_rx.recv())
        .await.expect("A remote delta").expect("channel open");

    let materialized_a: MaterializedMembership = {
        let s = state_a.lock().await;
        s.materialize_now(owner_a)
    };
    let materialized_b: MaterializedMembership = {
        let s = state_b.lock().await;
        s.materialize_now(owner_a)
    };
    let dto_a = member_info_for(&materialized_a);
    let dto_b = member_info_for(&materialized_b);
    assert_eq!(dto_a.len(), 2);
    assert_eq!(dto_b.len(), 2);
    assert_eq!(dto_a[0].addr, hex::encode(owner_a.0));
    assert_eq!(dto_a[0].power, 100);
    assert_eq!(dto_a[1].addr, hex::encode(owner_b.0));
    assert_eq!(dto_a[1].power, 0);
    assert_eq!(dto_a, dto_b);

    let leave_b = mint_leave_event(
        community_id, owner_b, &identity_b, "b-dev", 300_000, Some(&minted_b.bootstrap_join.at),
    ).expect("mint leave");
    engine_b.insert_local_event(leave_b).await.expect("B leave insert");

    assert!(wait_until(|| async {
        let s = state_a.lock().await;
        s.events.len() == 3
    }, Duration::from_secs(3)).await, "A should receive B's Leave");

    let materialized_a_after: MaterializedMembership = {
        let s = state_a.lock().await;
        s.materialize_now(owner_a)
    };
    let dto_after = member_info_for(&materialized_a_after);
    let b_row = dto_after.iter()
        .find(|d| d.addr == hex::encode(owner_b.0))
        .expect("B still in member list (Left, not removed)");
    assert_eq!(b_row.status, MemberStatus::Left);

    engine_a.shutdown().await.expect("shutdown a");
    engine_b.shutdown().await.expect("shutdown b");
}
```

If `mint_community_creation`, `mint_redemption`, `mint_leave_event`, `member_info_for`, `delta_to_change`, `MembershipChange` aren't `pub` from the crate root, ensure they are by re-exporting in `src-tauri/src/lib.rs`. Each test target sees the crate as `harmony_app` per the `[lib]` name in Cargo.toml.

If `IdentityResolver`, `DEFAULT_DEBOUNCE_MS`, `PersistPaths` aren't already `pub` from `community_state_sync`, the existing Phase 2 integration test (`community_sync_integration.rs`) confirms they are.

- [ ] **Step 2: Run test to verify it compiles + passes**

```bash
cd src-tauri
cargo fmt --all
set -o pipefail
cargo test --test community_open_flow_integration 2>&1 | tail -20
```

Expected: PASS. If it fails on visibility (`mint_*` not pub), add `pub` to the respective fns in `lib.rs`. If it fails on convergence timeout, increase the debounce-await tolerance via the `wait_until` deadline.

- [ ] **Step 3: Run all gates**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
set -o pipefail
cargo test --all-targets 2>&1 | tail -5
```

Expected: all green; total test count = previous + 1.

- [ ] **Step 4: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/tests/community_open_flow_integration.rs src-tauri/src/lib.rs
git commit -m "test(zeb-217-phase3): two-engine open community round-trip integration test"
```

---

## Task 13: Push branch + open Phase 3 PR

**Files:** none modified.

- [ ] **Step 1: Final pre-push gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
set -o pipefail
cargo test --all-targets 2>&1 | tail -5
cd /Users/zeblith/work/zeblithic/harmony-client
pnpm exec tsc --noEmit
pnpm exec vitest run
```

Expected: all green.

- [ ] **Step 2: Verify branch is up to date with origin/main**

```bash
git fetch origin main
git log --oneline origin/main..HEAD | head -20
git log --oneline HEAD..origin/main | head -20
```

Expected: HEAD has the Phase 3 commits ahead of `origin/main`; nothing in `origin/main` ahead of HEAD. If `origin/main` has advanced, rebase: `git rebase origin/main` and re-run all gates.

- [ ] **Step 3: Push branch**

```bash
git push -u origin zeb-217-sub-c-phase3-open-community-flow
```

- [ ] **Step 4: Open PR**

```bash
gh pr create --repo zeblithic/harmony-client \
  --title "feat(zeb-217): Sub-C Phase 3 — open community flow IPCs" \
  --body "$(cat <<'EOF'
## Summary

Phase 3 of ZEB-217 Sub-C ships the IPC layer for **open communities** end-to-end (no UI yet — Phase 5 lands the Svelte components). Five new IPC commands wired against the Phase 2 sync engine (PR #84, merge `466e6c2`):

- `create_community(name, isInviteOnly)` — mints fresh `community_id` + `membership_key`, writes the Community Space to owner-state, spawns the per-community engine via the registry, and inserts the bootstrap-admin self-Join event.
- `redeem_invite(url)` — decodes a `harmony://invite/...` URL, validates open path (rejects invite-only — Phase 4), writes the Space, spawns the engine, inserts the joiner's self-Join.
- `leave_community(communityId)` — mints a Leave event, inserts via the engine so peers receive it. Does not remove the Space (caller follows with the existing `remove_space` per spec).
- `list_community_members(communityId)` — returns the materialized member list sorted by power desc, joined_at asc.
- `generate_invite(communityId, ...)` — open path: emits a token-less `harmony://invite/{base64url}` URL.

Plus the supporting infrastructure:

- `CommunityMembershipDelta` channel from the engine — fires on every `InsertOutcome::Inserted` (own + remote).
- `run_community_delta_consumer` task at `start_node` — drains the channel, projects each delta into `MembershipChange`, emits `community-members-changed` Tauri event.
- `run_community_degraded_consumer` task at `start_node` — emits `community-state-sync-degraded` from Phase 2's already-shipped `error_tx`.
- `CommunitySyncEngine::insert_local_event` — single verify-then-insert-then-emit-delta-then-notify-publish path for IPC mutations; mirrors the engine's existing receive path.
- On-demand Zenoh adapter wiring via `community_adapter_request_tx` — `create_community` and `redeem_invite` push requests; `event_loop` spawns adapters mid-session.
- Snapshot-then-spawn fence on every CRDT-mutating IPC (mirrors `add_space` / `send_dm` from PR #81 round 6).

## Spec coverage

Implements the spec's "Open community redemption" flow + "IPC surface" §`create_community` / `redeem_invite` / `leave_community` / `list_community_members` / `generate_invite` (open path). All Phase 2 retrospective lessons inherited (no `.await` while holding state mutex, snapshot fence, sync I/O via `spawn_blocking`, `try_send` for fire-and-forget channels, distinct error variants per failure class).

## Out of scope (deferred per spec)

- Frontend / Svelte components → Phase 5
- Invite-only redemption (Reticulum counter-sig hop) → Phase 4
- `kick_from_community`, `set_power_level`, signed `InviteToken` for invite-only → Phase 4
- Deep-link plugin (`tauri-plugin-deep-link`) → Phase 5
- E2E Tauri::invoke harness → Phase 5 (ZEB-247)
- Cryptographic publisher authentication on state-root publishes → [ZEB-256](https://linear.app/zeblith/issue/ZEB-256/), required before Phase 4
- TreeKEM-style backward secrecy → [ZEB-249](https://linear.app/zeblith/issue/ZEB-249/)
- Persistent offline-counter-signer queue → [ZEB-254](https://linear.app/zeblith/issue/ZEB-254/)

## References

- Spec: `docs/specs/2026-05-05-zeb-217-sub-c-communities-design.md` (commit `0b84296`)
- Plan: `docs/plans/2026-05-06-zeb-217-sub-c-phase3-open-community-flow-plan.md`
- Phase 1 ship: PR #82, merge `bd1d01b`
- Phase 2 ship: PR #84, merge `466e6c2`
- Phase 2 spec refresh: PR #85, merge `0b84296`

## Test plan

- [ ] All Rust gates pass on CI: `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-targets`
- [ ] Frontend gates pass: `pnpm exec tsc --noEmit`, `pnpm exec vitest run`
- [ ] `community_open_flow_integration.rs` exercises create → redeem → list → leave end-to-end across two engines
- [ ] Each IPC has unit-test coverage for its inner `mint_*` helper
- [ ] `delta_to_change` covers all 5 `MembershipEventKind` variants (Phase 3 only fires Joined / Left in production; Kicked / Invited / PowerChanged covered by tests for forward-compat)
- [ ] `decode_invite_url` rejects wrong-scheme, bad-base64, and truncated-CBOR inputs

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 5: Verify PR opened cleanly**

```bash
gh pr view --repo zeblithic/harmony-client --json url,state,mergeable,mergeStateStatus
```

Expected: `state: OPEN`, `mergeable: MERGEABLE` (or `UNKNOWN` while CI runs). Watch CI in the PR — `Rust — fmt, clippy, test`, `MSRV`, `Frontend — tsc, vitest`, plus bot reviews (CodeRabbit, Cursor Bugbot, Qodo, Greptile, CodeAnt).

- [ ] **Step 6: No commit**

Task 13 ships through `git push` + `gh pr create`. No additional commit.

---

## Self-review

After writing this plan, the spec-coverage / placeholder / type-consistency check:

**1. Spec coverage:**

- ✅ `create_community` — Task 9
- ✅ `redeem_invite` (open) — Task 10
- ✅ `leave_community` — Task 11
- ✅ `list_community_members` — Task 5
- ✅ `generate_invite` (open) — Task 6
- ✅ `community-members-changed` event — Tasks 1, 7, 8
- ✅ `community-state-sync-degraded` event — Task 8 (Phase 2 emitter; Phase 3 wires consumer)
- ✅ Snapshot-then-spawn fence on every CRDT-mutating IPC — Tasks 9, 10, 11
- ✅ Bug-class coverage from Phase 2 retrospective — applied throughout (engine integration in Tasks 1-2, sync I/O via `spawn_blocking` already in shipped Phase 2, `try_send` for delta_tx, distinct error variants per failure class via `LocalInsertError` + `InviteUrlError`)

**2. Placeholder scan:** No `TBD`, `TODO`, "implement later", or "see Task X" cross-references. Every step has either real code or an exact command.

**3. Type consistency:**

- `MintedCommunity` (Task 9) reused by `mint_redemption` (Task 10) — both produce same shape downstream.
- `CommunityMembershipDelta` (Task 1) consumed by `delta_to_change` (Task 7) and `run_community_delta_consumer` (Task 8) — fields align.
- `CommunitySyncEngine::insert_local_event` (Task 2) called by `create_community` (Task 9), `redeem_invite` (Task 10), `leave_community` (Task 11) — same signature.
- `community_adapter_request_tx` introduced in Task 9, also used in Task 10 — wired through NodeState.
- `engine_arc` accessor introduced in Task 9, also used in Tasks 10 + 11.
- `MemberInfoDto` from Task 4 returned by `list_community_members` IPC in Task 5 and asserted on in Task 12.

If any subagent runs into a missing visibility (`pub` keyword on a fn / type), add it inline — the plan flags this risk in Task 12 Step 2 and provides the fix.

---

## Execution handoff

**Plan complete and saved to `docs/plans/2026-05-06-zeb-217-sub-c-phase3-open-community-flow-plan.md`. Two execution options:**

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, two-stage review (spec compliance + code quality) between tasks, fast iteration.

**2. Inline Execution** — Execute tasks in this session using `superpowers:executing-plans`, batch execution with checkpoints.

**Which approach?**
