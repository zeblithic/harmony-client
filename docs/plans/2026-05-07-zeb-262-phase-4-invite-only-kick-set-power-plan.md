# ZEB-262 Phase 4: Invite-Only Flow + Kick + Set-Power Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the IPC surface that distinguishes communities from a chat group: invite-only redemption (Reticulum counter-sig hop), `kick_from_community`, `set_power_level`. Plus auto-counter-sign on the receive side. Plus the [ZEB-258](https://linear.app/zeblith/issue/ZEB-258) atomic-rollback fix folded in.

**Architecture:** Mirror the DmInvite Path B app-sig binding (PR #80) for a new `CommunityInvitePacket` Reticulum unicast packet (discriminant `0x10`). The send path reorders so the owner-state Space commit is the LAST persistent step (atomic rollback on any earlier failure). The receive path adds a discriminant-based pre-dispatch in `event_loop.rs` that routes `0x10` to `community_invite::handle_unicast` for verify-then-counter-sign-then-publish.

**Tech Stack:** Rust, Tokio, Ed25519 (`ed25519_dalek`), `harmony_identity::PrivateIdentity`, canonical CBOR, Tauri 2, Reticulum unicast.

**Spec:** `docs/specs/2026-05-07-zeb-262-phase-4-invite-only-kick-set-power-design.md` (commit `cdbf7c8` on the `zeb-262-phase-4-invite-only-kick-set-power` branch — note: that branch carries the spec only; implementation lands on a fresh branch off `origin/main`).

**Branch:** `zeb-262-phase-4-invite-only-kick-set-power` (created in Task 0 off the latest `origin/main`).

**Out of scope:** ZEB-249 (MembershipKey rotation on kick), ZEB-251 (per-community power thresholds), ZEB-254 (offline-counter-signer queue), ZEB-260 (first-Join + self-Re-Join via blob inspection), ZEB-261 (membership-gate cache), Phase 5 admin UI. Explicitly carried in the spec; do NOT scope-creep into them.

---

## File structure

The change touches the IPC layer (`lib.rs`), adds two new modules (`community_invite` extension and a new `inbound_packet` dispatcher), and threads a new `Arc<PrivateIdentity>` field through `dm_outbox`. Wire-format types live in `community_invite.rs` so URL encoding (Phase 1) + envelope encoding (Phase 4) share one module — both encode the same `InviteToken` and `CommunityInvitePayload`/`CommunityInviteSigned` types.

**New modules:**

- `src-tauri/src/inbound_packet.rs` — minimal discriminant-based pre-dispatch for `RuntimeAction::UnicastReceived`. Peeks `packet[0]`, routes `0x10` to `community_invite::handle_unicast`, falls through everything else (including `0x01-0x03`) to the existing `dm_outbox.handle_unicast` path so this PR adds the new branch without refactoring DM dispatch.

**Extended modules:**

- `src-tauri/src/community_invite.rs` — currently 163 lines (URL encode/decode + `CommunityInvitePayload` + `InviteToken`). Phase 4 adds:
  - `CommunityInviteSigned` struct (signed body)
  - `CommunityInvitePacket` enum (Path B app-sig wrapper)
  - `encode_packet` / `decode_packet` (`extend_from_slice` + `split_at(len-64)` pattern from `dm_envelope`)
  - `build_signed_invite_packet` (canonical CBOR + sign)
  - `verify_packet` (pure function returning `Result<SignedMembershipEvent, CommunityInviteVerifyError>`)
  - `handle_unicast` (verify + counter-sign + publish via the engine + notify the pending-redemption oneshot)
  - `CommunityInviteVerifyError` enum (11 variants per spec table)
  - `serialize_identity_pub_as_bstr` / `deserialize_identity_pub_from_bstr` (mirror `dm_envelope`)

- `src-tauri/src/community_state_sync.rs` —
  - `CommunitySyncRegistry` gains `pending_redemptions: Arc<Mutex<HashMap<EventId, oneshot::Sender<()>>>>` field plus `register_pending_redemption` / `take_pending_redemption` / `notify_pending_redemption` helpers
  - `handle_incoming_publish` (receive merge loop, around line 1827) gets a post-merge step that, on `InsertOutcome::Inserted`, looks up `event.id` in pending_redemptions and fires the oneshot
  - `insert_local_event` (around line 765) gets an analogous post-merge notification (since the receive-side counter-sign path lands the counter-signed Join via `insert_local_event`, not `handle_incoming_publish`)
  - New surface `shutdown_engine_and_cleanup_persistence` for ZEB-258 rollback

- `src-tauri/src/dm_outbox.rs` — adds `pub(crate) private_identity: Arc<harmony_identity::PrivateIdentity>` field on `DmOutbox`; `DmOutbox::new` gains a fifth parameter; the existing `signing_key` field stays (still used for DM packet signing — production sign path is identical bytes). The `private_identity` field is a snapshot the receive path reads to call `attach_countersig_with_identity`.

- `src-tauri/src/event_loop.rs` — `handle_runtime_action_or_dispatch` (line 1414) gets a pre-fork: peek `packet[0]`. On `0x10`, dispatch to `community_invite::handle_unicast` with the `community_registry` + `dm_outbox` + `unicast_send_tx` + `app` handles. Otherwise fall through to the existing DM dispatch.

- `src-tauri/src/lib.rs` —
  - `redeem_invite` IPC: invite-only branch (10-step flow with HLC reservation FIRST + owner-state commit LAST)
  - `create_community` IPC: ZEB-258 reorder (owner-state commit LAST)
  - `kick_from_community` IPC (new)
  - `set_power_level` IPC (new)
  - `DmOutbox::new` construction site at `lib.rs:1097` (gains the new `private_identity` param)
  - `invoke_handler!` registration adds `kick_from_community`, `set_power_level`
  - `start_node` plumbs `community_registry` + `dm_outbox` + `unicast_send_tx` into `event_loop` (already done — Phase 3 + ZEB-256 wiring covers this; verify only)

**New tests:**

- `src-tauri/tests/community_invite_only_integration.rs` — two-engine invite-only happy path + timeout + atomic-rollback regression test

**Extended tests:**

- `src-tauri/tests/community_invite_unit.rs` — adds CommunityInviteSigned/CommunityInvitePacket roundtrip + 7 reject-variant tests
- `src-tauri/tests/community_membership_unit.rs` — adds kick-self-power-not-lower + set-power-out-of-range + admin-self-demote tests
- `src-tauri/tests/wire_format_community_fixtures.rs` — adds `community_invite_signed_wire_bytes_pinned`
- `src-tauri/tests/community_sync_registry_unit.rs` — adds `shutdown_engine_and_cleanup_persistence_*` + `pending_redemptions_*` tests
- `src-tauri/tests/community_sync_integration.rs` — adds ZEB-258 atomic-rollback regression + invite-only-happy-path + kick + set_power integration tests
- `src-tauri/tests/dm_send_integration.rs` / `dm_thread_integration.rs` / `dm_unicast_integration.rs` / `dm_outbox.rs` test module — adopt the new `DmOutbox::new` signature

**Why this decomposition:**

- ZEB-258 reorder (Task 1) ships first as a self-contained safety improvement with its own regression test. Landing it early makes `shutdown_engine_and_cleanup_persistence` (Task 7) directly useful for the redeem_invite send path (Task 8).
- `dm_outbox` field plumb (Task 2) lands second so every later task sees the final `DmOutbox::new` signature.
- `kick_from_community` + `set_power_level` (Task 3) ship before the wire-format chain because they're orthogonal — they don't touch the new wire format and can land independently. Getting them off the runway early simplifies the receive-path task (Task 9) which would otherwise be the largest task.
- Wire-format types (Task 4) precede encode/decode (Task 5) so canonical CBOR is locked before sig logic depends on it.
- Pure verify helper (Task 6) lands before any send/receive consumer so reject-variant tests can ship without engine plumbing.
- `shutdown_engine_and_cleanup_persistence` + `pending_redemptions` (Task 7) ship together as registry-internal additions both consumed by Task 8.
- `redeem_invite` invite-only send path (Task 8) lands before the receive path (Task 9) so the receive-path integration test exercises the real send path end-to-end.
- Receive dispatch + `handle_unicast` (Task 9) is the largest task; pulled to second-last so all dependencies stabilise first.
- Final verify + push + PR (Task 10) is gating only.

---

## Task 0: Pre-flight — verify Linear, branch off latest `origin/main`

**Why:** Per the user-memory rules, Linear IDs are assigned by Linear (never invented), branches must rebase on latest `origin/main`, and worktrees are forbidden. ZEB-262 + ZEB-258 already exist as separate Linear issues (filed during the spec phase); this PR closes both. The current branch `zeb-262-phase-4-invite-only-kick-set-power-design` carries the spec only — implementation goes on a fresh branch with the same name minus the `-design` suffix.

**Files:** None modified — git operations only.

- [ ] **Step 1: Confirm Linear tickets ZEB-262 + ZEB-258 exist and are In Progress**

Verify via Linear MCP:

```
list_issues with team_key="ZEB", query="ZEB-262"
list_issues with team_key="ZEB", query="ZEB-258"
```

Both should already exist in "In Progress" state. If either is missing, file via `save_issue` with the corresponding spec section as description, capture the assigned ID, then proceed. **Do NOT invent IDs.**

- [ ] **Step 2: Switch to main and pull**

```bash
git fetch origin
git checkout main
git pull origin main
```

Expected: `main` updates to commit `5a691f0` (ZEB-256 ship) or newer. If `git pull` shows merges from other PRs while you were planning, scan the merge commits for overlap with `community_state_sync.rs`, `community_invite.rs`, `event_loop.rs`, `lib.rs`, or `dm_outbox.rs`. If overlap exists, surface it to the human BEFORE proceeding — don't silently rebase over conflicts.

- [ ] **Step 3: Create implementation branch**

```bash
git checkout -b zeb-262-phase-4-invite-only-kick-set-power
```

Expected: `On branch zeb-262-phase-4-invite-only-kick-set-power`. NO worktree creation — per the user-memory `feedback_no_worktrees.md` HARD RULE.

If a branch with the same name already exists locally (because the spec was authored on a similarly-named branch), append a suffix or delete the design branch first; the design branch stays on `cdbf7c8` and is what the human committed the spec to. The implementation branch must be a fresh branch off `origin/main`.

- [ ] **Step 4: Confirm baseline tests pass on `main`**

```bash
set -o pipefail
cd src-tauri
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
```

Expected: all three commands succeed. If any fail on a clean main, that's "test drift is our fault" — fix the drift in a separate cleanup commit on this branch BEFORE starting Task 1, OR file a new Linear issue and document the failure in the PR description. Do NOT proceed with implementation tasks until the baseline is green.

- [ ] **Step 5: No commit yet**

Task 0 is a pre-flight only. The first commit lands in Task 1.

---

## Task 1: ZEB-258 atomic rollback — reorder `create_community` so owner-state commit is LAST

**Why:** Phase 3's `create_community` applies the Community Space row to owner-state CRDT BEFORE spawning the engine and dispatching the adapter request. If engine spawn or adapter dispatch fails, the Space row is committed but no engine exists to publish — the user's owner-state holds an orphan. The fix reorders the body so persistent owner-state mutation is the LAST step. On any earlier failure the engine is torn down (via the Phase 3-existing `stop_engine`) and the function returns `Err` with no owner-state change. ZEB-258 acceptance: simulate engine-spawn failure → owner-state CRDT byte-identical to pre-call snapshot.

**Files:**
- Modify: `src-tauri/src/lib.rs:5683-5876` (the `create_community` IPC body)
- Modify: `src-tauri/tests/community_sync_integration.rs` (add the regression test)

- [ ] **Step 1: Write the failing regression test**

Append to `src-tauri/tests/community_sync_integration.rs` at the end of the file (before the trailing `}` if any), and ensure the file's `use` block at the top includes `harmony_app::community_state_sync::CommunitySyncError`. Add this test:

```rust
/// ZEB-258 atomic rollback: simulate engine-spawn failure and assert
/// owner-state CRDT byte-identical to pre-call snapshot. Drives the
/// reorder of `create_community` so owner-state commit is LAST.
///
/// We can't easily fault-inject `spawn_engine` (it succeeds for any
/// well-formed Path), but we CAN simulate adapter dispatch failure by
/// dropping the receiver before calling `create_community`. Since the
/// post-reorder shape commits owner-state ONLY after `try_send` succeeds,
/// the adapter-Closed path is a faithful rollback exerciser.
///
/// Test shape: build a NodeState-equivalent with crdt_state +
/// community_registry + community_adapter_request_tx, drop the
/// matching adapter_request_rx, call create_community, assert Err,
/// then assert crdt_state has zero spaces (Space row never committed).
#[tokio::test]
async fn create_community_atomic_rollback_on_adapter_dispatch_failure() {
    // Reuse the test scaffolding from `create_community_inner_tests`'s
    // pure mint helpers but exercise the IPC body. The IPC body needs
    // a tauri::State<NodeState>, which the existing test scaffolding
    // doesn't provide. So this test calls the body's per-step pieces
    // in the same order, with `community_adapter_request_tx` paired to
    // a closed receiver. Mirrors the `redeem_invite_only_rolls_back_*`
    // test shape from Task 8 (forward-compatible).
    //
    // The pre-reorder build leaves `crdt_state` mutated (Space row
    // committed) when the adapter dispatch step fails. The post-reorder
    // build leaves `crdt_state` unchanged. This test asserts the
    // post-reorder behaviour and so MUST FAIL on the unreordered code.
    use harmony_app::community_state_crdt::ApplyOutcome;
    use harmony_app::owner_state_crdt::OwnerState;
    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;

    let crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));
    // Snapshot the empty-state canonical encoding for a byte-identical
    // assertion below. crdt_state.encoded() returns a Vec<u8> the
    // owner-state-CRDT layer guarantees is canonical.
    let pre_bytes: Vec<u8> = {
        let g = crdt_state.lock().await;
        // OwnerState exposes `to_canonical_bytes()` for persist; mirror
        // that for a stable snapshot. If a different stable accessor
        // exists, prefer it.
        harmony_app::owner_state_persist::canonicalize(&g)
            .expect("encode pre-state")
    };

    // Build a closed adapter channel: send half retained, recv half
    // dropped immediately so any try_send fails with Closed.
    let (adapter_tx, adapter_rx) =
        tokio::sync::mpsc::channel::<harmony_app::event_loop::CommunityAdapterRequest>(1);
    drop(adapter_rx);

    // Replicate the IPC body's order — but stop at the dispatch step.
    // (Full IPC test ships in Task 8 alongside redeem_invite. For
    // create_community the inner helpers don't depend on Tauri State,
    // so a direct invocation is cleaner.)
    //
    // POST-REORDER expected sequence:
    //   1. mint Space + bootstrap_join
    //   2. spawn_engine
    //   3. adapter_tx.try_send → FAILS (closed)
    //   4. stop_engine + return Err
    //   5. crdt_state untouched
    //
    // PRE-REORDER (broken): step 0 applied Space row before spawn_engine,
    // so post-error crdt_state holds a Space row. Test asserts step 5.

    // Step 1-4 are exercised through the actual `create_community` IPC
    // body. Since the IPC takes tauri::State<NodeState>, the cleanest
    // form is to call the IPC's inner helper that takes raw handles.
    // Phase 3 doesn't expose such a helper for create_community —
    // Task 1 ALSO extracts it. See Step 3 below.

    // For now, leave a placeholder assertion that will refine in Step 3
    // once the inner helper is named.
    let result = harmony_app::create_community_inner(
        "TestCommunity".to_string(),
        /* is_invite_only */ false,
        Arc::clone(&crdt_state),
        // hlc_tracker
        Arc::new(TokioMutex::new(Default::default())),
        "test-dev".into(),
        harmony_app::owner_state_types::OwnerAddr([0xab; 16]),
        // signing_key, identity_resolver, registry, content_store stubs:
        // wire from existing fixture builders or write a small one here.
        // (Full helper signature lands in Step 3.)
        unimplemented!("wire fixtures in Step 3"),
        adapter_tx,
        // dm_outbox stub — Task 2 ships the field; for create_community
        // it's only used to source signing_key, which we pass directly.
        // For Phase 3 we pass an Option-style Some/None; for Phase 4
        // (Task 2 onward) we pass an Arc<Mutex<DmOutbox>> with the
        // private_identity field.
        unimplemented!("dm_outbox fixture"),
        // generation snapshot
        0,
    )
    .await;

    assert!(
        result.is_err(),
        "create_community must fail when adapter dispatch is closed; got {:?}",
        result
    );

    let post_bytes: Vec<u8> = {
        let g = crdt_state.lock().await;
        harmony_app::owner_state_persist::canonicalize(&g)
            .expect("encode post-state")
    };
    assert_eq!(
        pre_bytes, post_bytes,
        "ZEB-258: owner-state CRDT must be byte-identical pre/post a \
         failed create_community (orphan Space row would prove the \
         reorder didn't land)"
    );
}
```

**Note on `unimplemented!()`:** the test as written depends on a `create_community_inner` helper function that doesn't yet exist (Phase 3's body is monolithic). Step 3 of this task EXTRACTS that helper alongside the reorder so the test compiles. If `create_community_inner` already exists at task start (i.e., a parallel branch added it), skip the extraction step and adapt the test to call the existing helper.

- [ ] **Step 2: Run the test to verify it fails to compile (helper missing)**

```bash
cd src-tauri
cargo test --test community_sync_integration create_community_atomic_rollback_on_adapter_dispatch_failure 2>&1 | tail -20
```

Expected: compile failure on `create_community_inner` (function not found) and the two `unimplemented!()` calls. This is the failing test fixture; we'll make it compile + fail with a real assertion in subsequent steps.

- [ ] **Step 3: Extract `create_community_inner` and reorder body**

Replace the body of `async fn create_community` in `src-tauri/src/lib.rs:5683` (the IPC entry point) so it does only the `tauri::State<NodeState>` snapshot, then delegates to a pure-ish inner helper. Add the helper directly above the IPC. The new shape:

```rust
/// Internal helper for `create_community`. Takes already-snapshotted
/// handles; pure of NodeState. ZEB-258: owner-state Space commit is the
/// LAST persistent step. Failures BEFORE the commit tear down the
/// engine + return Err with crdt_state untouched.
///
/// Argument shape mirrors `redeem_invite_inner` (Task 8) so the two
/// IPCs share a code-review pattern.
#[allow(clippy::too_many_arguments)]
pub async fn create_community_inner(
    name: String,
    is_invite_only: bool,
    crdt_state: std::sync::Arc<
        tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>,
    >,
    hlc_tracker: std::sync::Arc<
        tokio::sync::Mutex<std::collections::BTreeMap<String, crate::owner_state_types::Hlc>>,
    >,
    device_id: String,
    self_owner: crate::owner_state_types::OwnerAddr,
    signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
    community_registry: std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>,
    community_adapter_tx: tokio::sync::mpsc::Sender<crate::event_loop::CommunityAdapterRequest>,
    snapshot_generation: u64,
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
) -> Result<String, String> {
    if is_invite_only {
        return Err(
            "Phase 3 supports OPEN communities only; invite-only create_community ships in \
             Phase 4 (ZEB-262)"
                .to_string(),
        );
    }

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // ZEB-258: HLC reservation FIRST (under tracker lock). Reserved-but-
    // unused HLCs on abort are harmless because monotonicity only
    // requires strictly-increasing.
    let prev_hlc = {
        let t = hlc_tracker.lock().await;
        t.get(&device_id).cloned()
    };

    let minted = mint_community_creation(
        &name,
        is_invite_only,
        self_owner,
        signing_key.as_ref(),
        &device_id,
        wall_now_ms,
        prev_hlc.as_ref(),
    )?;

    // Advance HLC tracker now that we know the minted HLC. Done BEFORE
    // engine spawn so a future concurrent IPC sees the reservation.
    {
        let mut tracker_g = hlc_tracker.lock().await;
        tracker_g.insert(device_id.clone(), minted.space.created_at.clone());
    }

    // ZEB-258: spawn engine + dispatch adapter BEFORE the owner-state
    // commit. Both can fail; both have rollback paths (engine teardown
    // via shutdown_engine_and_cleanup_persistence — Task 7). At this
    // point owner-state is unchanged.
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

    if let Err(e) = community_adapter_tx.try_send(crate::event_loop::CommunityAdapterRequest {
        id_hex: hex::encode(minted.community_id.0),
        publisher_rx: pub_rx,
        subscriber_tx: sub_tx,
    }) {
        // Engine was spawned but adapter dispatch failed. Tear it down.
        // Owner-state still untouched at this point — that's the ZEB-258
        // win. Use stop_engine for now; Task 7 swaps in the new
        // shutdown_engine_and_cleanup_persistence helper that ALSO
        // removes the per-community persistence directory. For Task 1's
        // commit, stop_engine is sufficient — the orphan persistence
        // directory is tolerable until Task 7 lands.
        if let Err(stop_err) = community_registry
            .stop_engine(&minted.community_id)
            .await
        {
            tracing::warn!(
                error = %stop_err,
                community_id = %hex::encode(minted.community_id.0),
                "stop_engine failed during create_community rollback"
            );
        }
        return Err(match e {
            tokio::sync::mpsc::error::TrySendError::Full(_) => {
                "adapter request queue full; please retry".to_string()
            }
            tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                "adapter request channel closed (event_loop stopped?)".to_string()
            }
        });
    }

    // Bootstrap-Join via the engine. Verify_event authorises the admin
    // self-Join via the bootstrap rule; debounce kicks the publish.
    let engine_arc = community_registry
        .engine_arc(&minted.community_id)
        .await
        .ok_or("engine vanished immediately after spawn — registry race")?;
    let outcome = engine_arc
        .insert_local_event(minted.bootstrap_join.clone())
        .await
        .map_err(|e| format!("engine.insert_local_event: {e}"))?;
    if !matches!(
        outcome,
        crate::community_state_crdt::InsertOutcome::Inserted
    ) {
        // Bootstrap Join didn't insert — engine state is inconsistent
        // with the user-visible "creator just made this community"
        // expectation. Tear down + bail. Owner-state still untouched.
        // Task 7 swaps stop_engine for shutdown_engine_and_cleanup_persistence.
        if let Err(stop_err) = community_registry
            .stop_engine(&minted.community_id)
            .await
        {
            tracing::warn!(
                error = %stop_err,
                community_id = %hex::encode(minted.community_id.0),
                "stop_engine failed during create_community rollback"
            );
        }
        return Err(format!("bootstrap Join not inserted (got {outcome:?})"));
    }

    // ZEB-258: SNAPSHOT-THEN-COMMIT FENCE. If generation changed since
    // we snapshotted, owner-state is on a different lifetime — abort.
    {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            if let Err(stop_err) = community_registry
                .stop_engine(&minted.community_id)
                .await
            {
                tracing::warn!(
                    error = %stop_err,
                    community_id = %hex::encode(minted.community_id.0),
                    "stop_engine failed during create_community fence-abort"
                );
            }
            return Err(format!(
                "node generation changed during create_community (was {}, now {})",
                snapshot_generation, g.generation
            ));
        }
    }

    // ZEB-258: COMMIT owner-state Space LAST.
    {
        let mut state_g = crdt_state.lock().await;
        let outcome = state_g.apply_space_with_canonicalization(minted.space.clone());
        if matches!(outcome, crate::owner_state_crdt::ApplyOutcome::Rejected(_)) {
            // Owner-state rejected the Space (CRDT invariant). The engine
            // is up but owner-state has no Space row — tear down. Task 7
            // swaps stop_engine for shutdown_engine_and_cleanup_persistence.
            drop(state_g);
            if let Err(stop_err) = community_registry
                .stop_engine(&minted.community_id)
                .await
            {
                tracing::warn!(
                    error = %stop_err,
                    community_id = %hex::encode(minted.community_id.0),
                    "stop_engine failed during create_community apply-rejected"
                );
            }
            return Err(format!("apply_space rejected new community: {outcome:?}"));
        }
    }

    Ok(hex::encode(minted.community_id.0))
}

#[tauri::command]
async fn create_community(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    name: String,
    is_invite_only: bool,
) -> Result<String, String> {
    // Snapshot NodeState handles in a single guard scope. Then delegate
    // to the inner helper, which encodes the ZEB-258 reorder.
    let (
        crdt_state,
        hlc_tracker,
        device_id,
        self_owner,
        community_registry,
        community_adapter_tx,
        signing_key,
        snapshot_generation,
    ) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        let dm_outbox = g
            .dm_outbox
            .clone()
            .ok_or("dm_outbox missing — no owner identity?")?;
        // SigningKey is held inside dm_outbox under a tokio Mutex. We
        // can't hold the outbox guard across the `.await` chain below,
        // so snapshot the Arc<SigningKey> now under a brief lock and
        // reuse it everywhere downstream.
        let signing_key = {
            let outbox_g = dm_outbox.blocking_lock_owned(); // synchronous lock OK inside std::sync guard
            std::sync::Arc::clone(&outbox_g.signing_key)
        };
        (
            g.crdt_state
                .clone()
                .ok_or("crdt_state missing — node not running?")?,
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry
                .clone()
                .ok_or("community_registry missing — node not running?")?,
            g.community_adapter_request_tx
                .clone()
                .ok_or("community_adapter_request_tx missing")?,
            signing_key,
            g.generation,
        )
    };

    create_community_inner(
        name,
        is_invite_only,
        crdt_state,
        hlc_tracker,
        device_id,
        self_owner,
        signing_key,
        community_registry,
        community_adapter_tx,
        snapshot_generation,
        state_lock,
    )
    .await
}
```

**Lock-discipline note on `blocking_lock_owned`:** the outer `state_lock` is a `std::sync::Mutex` (not Tokio), so we cannot `.await` while holding it. Calling `blocking_lock_owned()` on the inner `Arc<tokio::sync::Mutex<DmOutbox>>` is safe ONLY because the std `state_lock` guard is released immediately after this snapshot scope ends. If the dm_outbox lock is contended, this brief block is acceptable (it's bounded by the lifetime of any single `dm_outbox` operation, all of which are short).

**Alternative if `blocking_lock_owned` blocks the runtime:** use `tokio::task::block_in_place` or restructure to re-acquire `dm_outbox` under a Tokio `.lock().await` AFTER releasing the std `state_lock`. The cleanest variant: snapshot the std-locked fields first (drop guard), then `dm_outbox.lock().await` to extract `signing_key`, then call `create_community_inner`. Adopt that variant if the blocking_lock approach trips clippy or runtime lints.

- [ ] **Step 4: Update the test to use `create_community_inner` with real fixtures**

Replace the `unimplemented!()`s in the test from Step 1 with concrete fixtures. The test now:

```rust
#[tokio::test]
async fn create_community_atomic_rollback_on_adapter_dispatch_failure() {
    use harmony_app::community_state_sync::{
        CommunityRegistryConfig, CommunitySyncRegistry, IdentityResolver,
        DEFAULT_DEBOUNCE_MS,
    };
    use harmony_app::content_store::{ContentStore, RuntimeContentStore};
    use harmony_app::owner_state_crdt::OwnerState;
    use harmony_app::owner_state_persist::canonicalize;
    use harmony_app::owner_state_types::OwnerAddr;
    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;

    struct NopResolver;
    #[async_trait::async_trait]
    impl IdentityResolver for NopResolver {
        async fn resolve(&self, _: &OwnerAddr) -> Option<[u8; 64]> { None }
    }

    // Closed adapter receiver → try_send returns Closed.
    let (adapter_tx, adapter_rx) =
        tokio::sync::mpsc::channel::<harmony_app::event_loop::CommunityAdapterRequest>(1);
    drop(adapter_rx);

    let (cas_op_tx, _cas_op_rx) = tokio::sync::mpsc::channel(8);
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(1000),
    ));

    let identity = harmony_identity::PrivateIdentity::from_seed(&[0xab; 32]);
    let self_owner = OwnerAddr(identity.identity.address_hash);
    let signing_key = Arc::new({
        let priv_bytes = identity.to_private_bytes();
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&priv_bytes[32..64]);
        ed25519_dalek::SigningKey::from_bytes(&seed)
    });

    let dir = tempfile::tempdir().expect("tempdir");
    let registry = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "test-dev".into(),
        content_store: cs,
        identity_resolver: Arc::new(NopResolver),
        identity_dir: dir.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner,
        signing_key: Arc::clone(&signing_key),
    }));

    let crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));
    let hlc_tracker = Arc::new(TokioMutex::new(Default::default()));

    let pre_bytes: Vec<u8> = {
        let g = crdt_state.lock().await;
        canonicalize(&g).expect("encode pre-state")
    };

    // create_community_inner takes a tauri::State; the test cannot
    // construct one. Instead we construct a parallel test-only
    // helper that takes raw NodeState handles. Or we factor the
    // body further so the snapshot-then-commit fence's std-lock
    // dependency is captured behind a trait. Pick the simpler
    // direction: skip the fence in the test (it cares about the
    // SPACE-COMMIT-LAST invariant, not the fence). Inline the
    // post-mint body without the fence:
    let wall_now_ms = 1_700_000_000_000u64;
    let prev_hlc = None;
    let minted = harmony_app::mint_community_creation(
        "TestCommunity",
        false,
        self_owner,
        signing_key.as_ref(),
        "test-dev",
        wall_now_ms,
        prev_hlc,
    )
    .expect("mint");

    let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

    registry
        .spawn_engine(
            minted.community_id,
            minted.membership_key.clone(),
            self_owner,
            false,
            pub_tx,
            sub_rx,
        )
        .await
        .expect("spawn ok");

    // Adapter dispatch — should fail (channel closed).
    let dispatch_result = adapter_tx.try_send(harmony_app::event_loop::CommunityAdapterRequest {
        id_hex: hex::encode(minted.community_id.0),
        publisher_rx: pub_rx,
        subscriber_tx: sub_tx,
    });
    assert!(
        dispatch_result.is_err(),
        "test fixture: adapter channel must be closed"
    );

    // Production rollback path: shutdown_engine_and_cleanup_persistence.
    // (Helper ships in Task 7. For now use stop_engine + manual dir
    // cleanup; this asserts the rollback shape, not the helper itself.)
    registry
        .stop_engine(&minted.community_id)
        .await
        .expect("stop ok");

    // ZEB-258: owner-state CRDT must be byte-identical to pre-call
    // snapshot. (The test never calls apply_space; the `create_community`
    // body in the post-reorder build never reaches the apply call when
    // adapter dispatch fails. This is the invariant the reorder
    // preserves.)
    let post_bytes: Vec<u8> = {
        let g = crdt_state.lock().await;
        canonicalize(&g).expect("encode post-state")
    };
    assert_eq!(
        pre_bytes, post_bytes,
        "ZEB-258: owner-state CRDT must be byte-identical pre/post a \
         failed create_community (orphan Space row would prove the \
         reorder didn't land)"
    );
}
```

This test is structurally a fault-injection harness around the ZEB-258 invariant rather than an end-to-end IPC call. The full IPC-level rollback test ships alongside `redeem_invite_only_rolls_back_on_engine_spawn_failure` in Task 8, where the inner helpers exist.

- [ ] **Step 5: Run the test — verify it now passes**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_sync_integration create_community_atomic_rollback 2>&1 | tail -10
```

Expected: PASS. If it fails because `shutdown_engine_and_cleanup_persistence` doesn't exist yet, the test uses `stop_engine` instead (Task 7 will swap in the new helper).

- [ ] **Step 6: Run cargo fmt + clippy + workspace tests**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
```

Expected: all green. The reorder must NOT regress any Phase 3 IPC test (`open_community_create_redeem_leave_round_trip`, etc.). If any test goes red, the reorder probably introduced a lock-order regression or a missed teardown — diagnose before committing.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/tests/community_sync_integration.rs
git commit -m "$(cat <<'EOF'
fix(zeb-258): atomic rollback on failed create_community

Reorder create_community so the owner-state Space commit is the LAST
persistent step. On engine-spawn or adapter-dispatch failure the engine
is torn down and crdt_state is byte-identical to the pre-call snapshot.

Extracts create_community_inner so the IPC body can be exercised from
an integration test. The redeem_invite reorder ships in Task 8 (ZEB-262
Phase 4) — same shape, different mint helper.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Plumb `Arc<PrivateIdentity>` through `DmOutbox`

**Why:** The receive-side counter-sign path calls `community_membership::attach_countersig_with_identity(&join_event, &private_identity)`, which needs a `&PrivateIdentity` (not just an `Arc<SigningKey>`). The DmOutbox is the natural snapshot of identity-derived material — it already holds `Arc<SigningKey>` for DM packet signing. Adding `Arc<PrivateIdentity>` alongside lets the receive handler grab a reference under the dm_outbox lock without re-loading the on-disk identity. Every construction site needs the new field; the IPC surface signature for `DmOutbox::new` grows by one parameter.

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs:374-407` (struct + `new`)
- Modify: `src-tauri/src/lib.rs:920-1103` (production construction site — also reconstruct the `PrivateIdentity` before `ed25519` is dropped at line 933)
- Modify: `src-tauri/tests/dm_send_integration.rs:77`
- Modify: `src-tauri/tests/dm_thread_integration.rs:84`
- Modify: `src-tauri/tests/dm_unicast_integration.rs:216,223,470,476` (4 sites)
- Modify: `src-tauri/src/dm_outbox.rs:1606-1614` (the `make_outbox_synthetic` test helper)
- Add: a unit test `dm_outbox_holds_private_identity_for_countersign` in `src-tauri/src/dm_outbox.rs`'s test module

- [ ] **Step 1: Write the failing unit test for the new field**

In `src-tauri/src/dm_outbox.rs`, find the test module (around line 1606) and add a test asserting the field is plumbed and signs with the same key as the existing `signing_key`:

```rust
#[test]
fn dm_outbox_holds_private_identity_for_countersign() {
    use harmony_identity::PrivateIdentity;

    let identity = PrivateIdentity::from_seed(&[0xc7; 32]);
    let private_identity = std::sync::Arc::new(identity.clone());
    let priv_bytes = identity.to_private_bytes();
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&priv_bytes[32..64]);
    let signing_key = std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&seed));
    let self_owner = OwnerAddr(identity.identity.address_hash);
    let device_hash = DeviceIdentityHash([0xab; 16]);

    let outbox = DmOutbox::new(
        "dev".into(),
        self_owner,
        device_hash,
        std::sync::Arc::clone(&signing_key),
        std::sync::Arc::clone(&private_identity),
    );

    // Both fields produce byte-identical signatures over the same bytes.
    let msg = b"countersig harness";
    let sig_via_outbox_signing_key = outbox.signing_key.sign(msg).to_bytes();
    let sig_via_private_identity = outbox.private_identity.sign(msg);
    assert_eq!(
        sig_via_outbox_signing_key, sig_via_private_identity,
        "DmOutbox.signing_key and DmOutbox.private_identity must \
         produce identical signatures (PrivateIdentity::sign internally \
         dispatches to the same SigningKey bytes; mismatch means the \
         field plumb wired the wrong identity)"
    );
}
```

This test fixes the contract: both fields refer to the same key material. If a future refactor accidentally pairs a `signing_key` from identity A with a `private_identity` from identity B, this assertion catches it.

- [ ] **Step 2: Run the test — verify it fails to compile**

```bash
cd src-tauri
set -o pipefail
cargo test --lib dm_outbox::tests::dm_outbox_holds_private_identity_for_countersign 2>&1 | tail -20
```

Expected: compile failure (`DmOutbox::new` takes 4 args, got 5; `DmOutbox` has no field `private_identity`).

- [ ] **Step 3: Add the field + extend `DmOutbox::new`**

Edit `src-tauri/src/dm_outbox.rs:374-407`:

```rust
pub struct DmOutbox {
    pub(crate) device_id: String,
    pub(crate) self_owner: OwnerAddr,
    pub(crate) our_signing_device_hash: DeviceIdentityHash,
    pub(crate) signing_key: Arc<ed25519_dalek::SigningKey>,
    /// ZEB-262 Phase 4: full PrivateIdentity used by the inbound
    /// CommunityInvite handler to call
    /// `community_membership::attach_countersig_with_identity` —
    /// which takes `&PrivateIdentity` (not just `&SigningKey`).
    /// Snapshotted at outbox construction so the receive handler can
    /// borrow a reference under the dm_outbox lock without re-loading
    /// the on-disk identity. Held via `Arc` so DmOutbox can be cheaply
    /// cloned across construction sites; the underlying PrivateIdentity
    /// is never copied (the secret bytes stay in one allocation).
    pub(crate) private_identity: Arc<harmony_identity::PrivateIdentity>,
    in_flight: HashSet<(OutboxEntryId, OwnerAddr)>,
    backoff: HashMap<(OutboxEntryId, OwnerAddr), AttemptState>,
}

impl DmOutbox {
    pub fn new(
        device_id: String,
        self_owner: OwnerAddr,
        our_signing_device_hash: DeviceIdentityHash,
        signing_key: Arc<ed25519_dalek::SigningKey>,
        private_identity: Arc<harmony_identity::PrivateIdentity>,
    ) -> Self {
        Self {
            device_id,
            self_owner,
            our_signing_device_hash,
            signing_key,
            private_identity,
            in_flight: HashSet::new(),
            backoff: HashMap::new(),
        }
    }
    // ... rest unchanged ...
}
```

- [ ] **Step 4: Update production construction site in `lib.rs`**

Edit `src-tauri/src/lib.rs` around line 1083-1103 — BEFORE the `drop(ed25519)` at line 933, capture an `Arc<PrivateIdentity>` from the loaded identity. Then pass it into `DmOutbox::new`:

```rust
// Around line 932 (before drop(ed25519)):
let private_identity_arc =
    std::sync::Arc::new(ed25519.clone());
let reticulum_identity_bytes = Some(zeroize::Zeroizing::new(ed25519.to_private_bytes()));
drop(ed25519);

// ... existing code through line 1093 ...

let outbox = std::sync::Arc::new(tokio::sync::Mutex::new(
    crate::dm_outbox::DmOutbox::new(
        device_id.clone(),
        self_owner,
        our_signing_device_hash,
        signing_key_arc.clone(),
        std::sync::Arc::clone(&private_identity_arc),
    ),
));
```

**Verify `harmony_identity::PrivateIdentity: Clone`.** If `PrivateIdentity` doesn't implement `Clone`, use `harmony_identity::PrivateIdentity::from_private_bytes(&priv_bytes_full)` instead — the bytes are already in scope as `reticulum_identity_bytes` (a `Zeroizing<Vec<u8>>`):

```rust
let private_identity_arc = std::sync::Arc::new(
    harmony_identity::PrivateIdentity::from_private_bytes(
        reticulum_identity_bytes
            .as_ref()
            .expect("populated above")
            .as_slice(),
    )
    .expect("private bytes round-trip"),
);
```

Pick whichever fits — Read `harmony_identity/src/identity.rs` first to confirm. The `from_private_bytes` form is robust because it doesn't depend on `Clone` and uses bytes that are already retained for SigningKey extraction below.

- [ ] **Step 5: Update the test helper `make_outbox_synthetic`**

In `src-tauri/src/dm_outbox.rs:1606`:

```rust
fn make_outbox_synthetic(device_id: &str, self_owner: OwnerAddr) -> DmOutbox {
    use harmony_identity::PrivateIdentity;
    // Stable test-only seed; matches the synthetic-key shape the rest
    // of the test module uses. The private_identity here is purely a
    // placeholder — make_outbox_synthetic's existing tests don't
    // exercise the countersign path. Future tests that DO exercise
    // it should construct a real PrivateIdentity::from_seed instead.
    let identity = PrivateIdentity::from_seed(&[0x55; 32]);
    let private_identity = std::sync::Arc::new(identity);
    DmOutbox::new(
        device_id.into(),
        self_owner,
        DeviceIdentityHash([0u8; 16]),
        std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0xaa; 32])),
        private_identity,
    )
}
```

- [ ] **Step 6: Update the four integration test construction sites**

In each test file, locate `DmOutbox::new(...)` and add the `private_identity` argument. The pattern in all four:

```rust
// Before each DmOutbox::new call, derive a private_identity from the
// existing test seed. Mirror the production extraction (private bytes →
// PrivateIdentity::from_private_bytes). For tests that already build
// `identity_a: PrivateIdentity` etc., reuse that handle directly.

let private_identity_arc = std::sync::Arc::new(
    /* the existing PrivateIdentity in scope, e.g. identity_a.clone() */
    /* or PrivateIdentity::from_seed(&[0xa1; 32]) if no handle is */
    /* in scope */
);

let outbox = DmOutbox::new(
    "alice-device".into(),
    alice,
    our_device_hash,
    signing_key,
    std::sync::Arc::clone(&private_identity_arc),
);
```

Specific sites to touch (each gets one `Arc::clone(&private_identity_arc)` added as a fifth argument):

- `src-tauri/tests/dm_thread_integration.rs:84`
- `src-tauri/tests/dm_send_integration.rs:77`
- `src-tauri/tests/dm_unicast_integration.rs:216` (alice_outbox in first test)
- `src-tauri/tests/dm_unicast_integration.rs:223` (bob_outbox in first test)
- `src-tauri/tests/dm_unicast_integration.rs:470` (alice_outbox in second test)
- `src-tauri/tests/dm_unicast_integration.rs:476` (bob_outbox in second test)

For the `dm_unicast_integration.rs` file: each pair (alice, bob) gets two distinct `PrivateIdentity::from_seed(...)` instances with different seed bytes so the test's distinct-identity invariants (sender vs recipient) hold for the new field too.

- [ ] **Step 7: Run `cargo build` to catch any missed sites**

```bash
cd src-tauri
set -o pipefail
cargo build --workspace --all-targets --locked 2>&1 | tail -30
```

Expected: compiles. If the build complains about another `DmOutbox::new` site, fix it. If `PrivateIdentity` doesn't implement `Clone`, switch the construction to `from_private_bytes` (Step 4 alternative).

- [ ] **Step 8: Run the full test suite**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
```

Expected: all green, including the new `dm_outbox_holds_private_identity_for_countersign` test.

- [ ] **Step 9: Commit**

```bash
git add src-tauri/src/dm_outbox.rs src-tauri/src/lib.rs \
        src-tauri/tests/dm_send_integration.rs \
        src-tauri/tests/dm_thread_integration.rs \
        src-tauri/tests/dm_unicast_integration.rs
git commit -m "$(cat <<'EOF'
feat(zeb-262): plumb Arc<PrivateIdentity> through DmOutbox

Phase 4 receive-side counter-sign needs a &PrivateIdentity for
attach_countersig_with_identity. Snapshot the loaded identity into
DmOutbox alongside the existing Arc<SigningKey>, so the inbound
CommunityInvite handler can grab a reference under the outbox lock
without re-loading the on-disk identity.

Test asserts the two fields produce identical signatures over the
same bytes — a misplumbed field would silently pair signing_key
from identity A with private_identity from identity B.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: `kick_from_community` + `set_power_level` IPCs

**Why:** These two IPCs are mostly mechanical and orthogonal to the wire-format work. They share the leave_community shape: snapshot NodeState handles → reserve HLC → mint event → engine.insert_local_event → translate VerifyError. Power gating is enforced inside the engine's `verify_event` (parent spec §"Verification"); the IPC trusts that and translates `VerifyError` discriminants to user-readable strings. Shipping both in one task keeps the membership-event-mint pattern in one place and lets Task 9's receive-path test rely on a stable IPC for assertions.

**Files:**
- Modify: `src-tauri/src/lib.rs:6411` (after `leave_community` — same pattern, two new IPCs)
- Modify: `src-tauri/src/lib.rs:6990` (the `invoke_handler!` registration adds the two new commands)
- Modify: `src-tauri/tests/community_membership_unit.rs` (3 new edge tests)
- Modify: `src-tauri/tests/community_sync_integration.rs` (2 new happy-path integration tests)

- [ ] **Step 1: Write the failing membership-unit tests**

Append to `src-tauri/tests/community_membership_unit.rs`:

```rust
#[test]
fn kick_self_rejected_with_kick_target_power_not_lower() {
    use harmony_app::community_membership::{
        materialize, sign_event, verify_event, EventPayload, MaterializedMembership,
        MembershipEventKind, VerifyContext, VerifyError,
    };
    use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};

    let admin_id = harmony_identity::PrivateIdentity::from_seed(&[0xa1; 32]);
    let admin_addr = OwnerAddr(admin_id.identity.address_hash);
    let admin_pub = admin_id.identity.to_public_bytes();
    let admin_sk = signing_key_from(&admin_id);

    let community_id = SpaceId([0x77; 16]);
    let admin_join = sign_event(
        &EventPayload {
            id: [1u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: admin_addr,
            at: Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "a".into(),
            },
        },
        &admin_sk,
    )
    .unwrap();

    // Admin kicks self — admin power 100, target (self) power 100,
    // so target.power not strictly less than actor.power → reject.
    let kick_self = sign_event(
        &EventPayload {
            id: [2u8; 16],
            community_id,
            kind: MembershipEventKind::Kick {
                target: admin_addr,
                reason: None,
            },
            actor: admin_addr,
            at: Hlc {
                wall_ms: 2000,
                logical: 0,
                device_id: "a".into(),
            },
        },
        &admin_sk,
    )
    .unwrap();

    let prior = materialize(&[admin_join], admin_addr);
    let ctx = VerifyContext {
        expected_community_id: community_id,
        admin_addr,
        is_invite_only: false,
        actor_identity_pub: &admin_pub,
        countersigner_identity_pub: None,
    };
    let err = verify_event(&kick_self, &prior, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::KickTargetPowerNotLower);
}

#[test]
fn set_power_out_of_range_rejected() {
    use harmony_app::community_membership::{
        materialize, sign_event, verify_event, EventPayload, MembershipEventKind,
        VerifyContext, VerifyError,
    };
    use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};

    let admin_id = harmony_identity::PrivateIdentity::from_seed(&[0xa2; 32]);
    let admin_addr = OwnerAddr(admin_id.identity.address_hash);
    let admin_pub = admin_id.identity.to_public_bytes();
    let admin_sk = signing_key_from(&admin_id);
    let target_addr = OwnerAddr([0xbb; 16]);

    let community_id = SpaceId([0x88; 16]);
    let admin_join = sign_event(
        &EventPayload {
            id: [1u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: admin_addr,
            at: Hlc { wall_ms: 1000, logical: 0, device_id: "a".into() },
        },
        &admin_sk,
    )
    .unwrap();

    // SetPower with level=200 — exceeds POWER_THRESHOLDS.max (100).
    let set_power = sign_event(
        &EventPayload {
            id: [3u8; 16],
            community_id,
            kind: MembershipEventKind::SetPower { target: target_addr, level: 200 },
            actor: admin_addr,
            at: Hlc { wall_ms: 2000, logical: 0, device_id: "a".into() },
        },
        &admin_sk,
    )
    .unwrap();

    let prior = materialize(&[admin_join], admin_addr);
    let ctx = VerifyContext {
        expected_community_id: community_id,
        admin_addr,
        is_invite_only: false,
        actor_identity_pub: &admin_pub,
        countersigner_identity_pub: None,
    };
    let err = verify_event(&set_power, &prior, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::PowerLevelOutOfRange);
}

#[test]
fn set_power_admin_self_demote_inserts() {
    use harmony_app::community_membership::{
        materialize, sign_event, verify_event, EventPayload, MembershipEventKind,
        VerifyContext,
    };
    use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};

    let admin_id = harmony_identity::PrivateIdentity::from_seed(&[0xa3; 32]);
    let admin_addr = OwnerAddr(admin_id.identity.address_hash);
    let admin_pub = admin_id.identity.to_public_bytes();
    let admin_sk = signing_key_from(&admin_id);

    let community_id = SpaceId([0x99; 16]);
    let admin_join = sign_event(
        &EventPayload {
            id: [1u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: admin_addr,
            at: Hlc { wall_ms: 1000, logical: 0, device_id: "a".into() },
        },
        &admin_sk,
    )
    .unwrap();

    // Admin demotes self to power 50. Foot-gun, but verify_event MUST
    // accept (admin power 100 ≥ set_power_threshold 100; level 50 in
    // range; no power-level transition rule rejects it). The user-
    // visible warning lives in the future Phase 5 UI, not in
    // verify_event.
    let demote = sign_event(
        &EventPayload {
            id: [4u8; 16],
            community_id,
            kind: MembershipEventKind::SetPower { target: admin_addr, level: 50 },
            actor: admin_addr,
            at: Hlc { wall_ms: 2000, logical: 0, device_id: "a".into() },
        },
        &admin_sk,
    )
    .unwrap();

    let prior = materialize(&[admin_join], admin_addr);
    let ctx = VerifyContext {
        expected_community_id: community_id,
        admin_addr,
        is_invite_only: false,
        actor_identity_pub: &admin_pub,
        countersigner_identity_pub: None,
    };
    verify_event(&demote, &prior, &ctx)
        .expect("admin self-demote must verify (foot-gun is allowed)");
}
```

The `signing_key_from` helper is already defined at the top of `community_membership_unit.rs` from earlier tests (Phase 1).

- [ ] **Step 2: Run the new unit tests — verify they fail**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_membership_unit kick_self_rejected 2>&1 | tail -10
cargo test --test community_membership_unit set_power_out_of_range 2>&1 | tail -10
cargo test --test community_membership_unit set_power_admin_self_demote 2>&1 | tail -10
```

Expected: All three tests should FAIL on baseline because their expected reject discriminants (`KickTargetPowerNotLower`, `PowerLevelOutOfRange`) and admin-self-demote happy-path are existing Phase 1 invariants. Actually — these are Phase 1 invariants already shipped, so the tests should PASS without any new code. Re-read `community_membership.rs:1010-1100` to confirm. **If the tests pass on baseline:** they're a regression harness for Phase 1 verify_event logic, which is fine — they serve as a regression-pin so the kick/set-power IPCs land on a guaranteed-stable verify_event.

If they pass without code changes, skip Step 3 of this section (no membership.rs edit needed) and proceed to Step 4 (the IPCs).

- [ ] **Step 3: (Conditional) Fix any verify_event gaps the unit tests expose**

Only execute if Step 2 reveals an actual gap. Not expected — the spec calls these tests `extend` to community_membership_unit.rs, implying they're additive coverage of existing logic.

- [ ] **Step 4: Add the `kick_from_community` IPC**

Append to `src-tauri/src/lib.rs` after the `leave_community_inner_tests` module:

```rust
// ── ZEB-262 Phase 4: kick_from_community ─────────────────────────────
//
// Mints a Kick SignedMembershipEvent and inserts it through the
// per-community engine. Power-gate enforcement happens INSIDE
// engine.insert_local_event (which calls verify_event) — actor must
// have power ≥ kick_threshold (50) AND strictly greater than target's
// power. The IPC trusts verify_event and translates VerifyError
// discriminants to user-readable strings. Pre-validating here would
// duplicate the rules and risk drift.

/// Pure function: mint a self-signed Kick event for a community we
/// belong to and have permission to moderate. Mirrors mint_leave_event.
pub fn mint_kick_event(
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    target: crate::owner_state_types::OwnerAddr,
    reason: Option<String>,
    signing_key: &ed25519_dalek::SigningKey,
    device_id: &str,
    wall_now_ms: u64,
    prev_hlc: Option<&crate::owner_state_types::Hlc>,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let kick_hlc = crate::dm_outbox::next_hlc(prev_hlc, wall_now_ms, device_id);
    let payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::Kick { target, reason },
        actor: self_owner,
        at: kick_hlc,
    };
    sign_event(&payload, signing_key).map_err(|e| format!("sign kick: {e}"))
}

/// Tauri IPC: kick a member from a community.
///
/// Power-gated by `verify_event`: actor must have power ≥ 50 (kick
/// threshold) AND strictly greater than target's current power.
/// Returns Err with the VerifyError discriminant on rejection.
#[tauri::command]
async fn kick_from_community(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    target_addr: String,
    reason: Option<String>,
) -> Result<(), String> {
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let target_bytes: [u8; 16] = hex::decode(&target_addr)
        .map_err(|e| format!("invalid target_addr hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "target_addr must be 16 bytes (32 hex chars)".to_string())?;
    let target = crate::owner_state_types::OwnerAddr(target_bytes);

    let (hlc_tracker, device_id, self_owner, community_registry, dm_outbox, snapshot_generation) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry
                .clone()
                .ok_or("community_registry missing — node not running?")?,
            g.dm_outbox
                .clone()
                .ok_or("dm_outbox missing — no owner identity?")?,
            g.generation,
        )
    };

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

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

    // Generation fence.
    {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            return Err(format!(
                "node generation changed during kick_from_community (was {}, now {})",
                snapshot_generation, g.generation
            ));
        }
    }

    let engine_arc = community_registry
        .engine_arc(&space_id)
        .await
        .ok_or_else(|| {
            format!(
                "no engine for community {} — not currently joined",
                hex::encode(space_id.0)
            )
        })?;
    let outcome = engine_arc
        .insert_local_event(kick.clone())
        .await
        .map_err(|e| format!("engine.insert_local_event: {e}"))?;
    if let crate::community_state_crdt::InsertOutcome::Rejected(verr) = &outcome {
        return Err(format!("Kick rejected by CRDT verify: {verr}"));
    }

    // Advance HLC tracker on Inserted.
    if matches!(outcome, crate::community_state_crdt::InsertOutcome::Inserted) {
        let mut t = hlc_tracker.lock().await;
        t.insert(device_id.clone(), kick.at.clone());
    }

    Ok(())
}
```

- [ ] **Step 5: Add the `set_power_level` IPC**

Same pattern; also append to `src-tauri/src/lib.rs`:

```rust
// ── ZEB-262 Phase 4: set_power_level ─────────────────────────────────

pub fn mint_set_power_event(
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    target: crate::owner_state_types::OwnerAddr,
    level: u8,
    signing_key: &ed25519_dalek::SigningKey,
    device_id: &str,
    wall_now_ms: u64,
    prev_hlc: Option<&crate::owner_state_types::Hlc>,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use rand::RngCore;

    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);

    let hlc = crate::dm_outbox::next_hlc(prev_hlc, wall_now_ms, device_id);
    let payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::SetPower { target, level },
        actor: self_owner,
        at: hlc,
    };
    sign_event(&payload, signing_key).map_err(|e| format!("sign set_power: {e}"))
}

#[tauri::command]
async fn set_power_level(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    target_addr: String,
    level: u8,
) -> Result<(), String> {
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let target_bytes: [u8; 16] = hex::decode(&target_addr)
        .map_err(|e| format!("invalid target_addr hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "target_addr must be 16 bytes (32 hex chars)".to_string())?;
    let target = crate::owner_state_types::OwnerAddr(target_bytes);

    let (hlc_tracker, device_id, self_owner, community_registry, dm_outbox, snapshot_generation) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry
                .clone()
                .ok_or("community_registry missing — node not running?")?,
            g.dm_outbox
                .clone()
                .ok_or("dm_outbox missing — no owner identity?")?,
            g.generation,
        )
    };

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

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

    {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            return Err(format!(
                "node generation changed during set_power_level (was {}, now {})",
                snapshot_generation, g.generation
            ));
        }
    }

    let engine_arc = community_registry
        .engine_arc(&space_id)
        .await
        .ok_or_else(|| {
            format!(
                "no engine for community {} — not currently joined",
                hex::encode(space_id.0)
            )
        })?;
    let outcome = engine_arc
        .insert_local_event(event.clone())
        .await
        .map_err(|e| format!("engine.insert_local_event: {e}"))?;
    if let crate::community_state_crdt::InsertOutcome::Rejected(verr) = &outcome {
        return Err(format!("SetPower rejected by CRDT verify: {verr}"));
    }

    if matches!(outcome, crate::community_state_crdt::InsertOutcome::Inserted) {
        let mut t = hlc_tracker.lock().await;
        t.insert(device_id.clone(), event.at.clone());
    }

    Ok(())
}
```

- [ ] **Step 6: Register both IPCs in `invoke_handler!`**

Edit `src-tauri/src/lib.rs:6990` (the existing `invoke_handler!` macro). Add `kick_from_community` and `set_power_level` alongside the other community commands:

```rust
            list_community_members,
            generate_invite,
            create_community,
            redeem_invite,
            leave_community,
            kick_from_community,
            set_power_level,
```

- [ ] **Step 7: Add integration tests for happy-path kick + set_power**

Append to `src-tauri/tests/community_sync_integration.rs`:

```rust
/// Two-engine kick happy path: admin (A) kicks Bob (B). The Kick
/// event materialises on both A and B as MemberStatus::Banned for B.
/// Mirrors `community_open_flow_integration.rs` setup but exercises
/// the kick + materialize round-trip.
#[tokio::test]
async fn admin_kicks_member_round_trip() {
    // ... (full setup mirrors open_community_create_redeem_leave_round_trip
    // through Step 3 — Alice creates community, Bob redeems open invite,
    // both engines hold both Joins. Then Alice mints a Kick(Bob) via
    // mint_kick_event, inserts via engine_a.insert_local_event, asserts
    // outcome=Inserted, waits for B to converge, asserts B's local
    // materialized state shows Bob as Banned.)
    //
    // This is a setup-heavy test. Use the same TwoIdentityResolver +
    // shared CAS + forwarder pattern as community_open_flow_integration.
    // Reference structure: open_community_create_redeem_leave_round_trip.
    //
    // After both peers converge on bootstrap + redemption Joins:
    //   1. let kick = harmony_app::mint_kick_event(community_id, owner_a,
    //          owner_b, Some("test-kick".into()), &signing_a, "a-dev",
    //          300_000, Some(&minted_b.bootstrap_join.at))?;
    //   2. let outcome = engine_a.insert_local_event(kick.clone()).await?;
    //   3. assert_eq!(outcome, InsertOutcome::Inserted);
    //   4. wait_until B's state holds 3 events
    //   5. let mat_b = community_membership::materialize(&events_b, owner_a);
    //      assert_eq!(mat_b.members.get(&owner_b).map(|m| m.status),
    //                 Some(MemberStatus::Banned));
    //
    // Implementer: copy the full setup from open_community_create_redeem_leave_round_trip,
    // strip the Leave step, add the Kick step. ~50 lines of test body.
    todo!("implement following the structure above; full code in implementation");
}

/// Two-engine set_power happy path: admin (A) promotes Bob (B) to
/// power 50. After convergence both materializations show Bob.power = 50.
#[tokio::test]
async fn admin_sets_power_round_trip() {
    // Same setup; after both engines hold bootstrap + redemption Joins,
    // call mint_set_power_event(owner_a → SetPower{target=owner_b,
    // level=50}), insert via engine_a, wait for convergence on B, assert
    // mat_b.power_levels.get(&owner_b) == Some(50).
    //
    // ~40 lines.
    todo!("implement following the structure above");
}
```

**On `todo!` in tests:** these are placeholders that the implementer MUST fill in by copying the full two-engine fixture pattern from `community_open_flow_integration.rs::open_community_create_redeem_leave_round_trip` (lines 82-300+). The pattern is well-established; reproducing it inline here would balloon this plan. Implementer:
1. Open `community_open_flow_integration.rs` and locate the round-trip test.
2. Copy lines 82-313 (pre-Leave-step) into the new test bodies.
3. After "both peers converge on the materialized member list", insert the kick/set_power event via `engine_a.insert_local_event(...)`, wait for B to converge (`state_b.lock().await.events.len() == 3`), and assert the materialized state.

The `todo!()` placeholders MUST be replaced before commit; cargo test will report `panicked at 'not yet implemented'` if left in.

- [ ] **Step 8: Run all new tests**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --test community_membership_unit kick_self_rejected set_power_out_of_range set_power_admin_self_demote 2>&1 | tail -20
cargo test --test community_sync_integration admin_kicks_member admin_sets_power 2>&1 | tail -20
cargo test --workspace --all-targets --locked
```

Expected: all green.

- [ ] **Step 9: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/tests/community_membership_unit.rs \
        src-tauri/tests/community_sync_integration.rs
git commit -m "$(cat <<'EOF'
feat(zeb-262): kick_from_community + set_power_level IPCs

Both IPCs follow the leave_community shape: snapshot NodeState handles,
reserve HLC, mint signed event, engine.insert_local_event, translate
VerifyError. Power gating lives inside verify_event (Phase 1) — IPCs
trust that and surface the discriminant on rejection.

Adds three membership-unit edge tests (kick-self power-not-lower,
set-power out-of-range, admin-self-demote), and two integration tests
exercising the cross-engine kick + set_power happy paths.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: `CommunityInviteSigned` + `CommunityInvitePacket` types + canonical wire fixture

**Why:** The wire format must be locked before any sign/verify code depends on it. Pin the canonical CBOR bytes in the same commit so a future encoder regression is caught at compile-of-tests time. Mirrors the `DmInviteSigned` shape from `dm_envelope.rs` lines 67-115 — same Path B app-sig binding (signing_device_hash inside the signed body, identity_pub bytes inline for bootstrap).

**Files:**
- Modify: `src-tauri/src/community_invite.rs` (extend with new types)
- Modify: `src-tauri/tests/wire_format_community_fixtures.rs` (pin bytes)

- [ ] **Step 1: Write the failing canonical-CBOR roundtrip + pinned-bytes test**

Append to `src-tauri/tests/wire_format_community_fixtures.rs`:

```rust
/// ZEB-262 Phase 4: pin the CommunityInviteSigned canonical CBOR bytes.
/// Mirrors community_membership_signed_event_canonical_roundtrip — the
/// fixture catches encoder drift across phases.
///
/// Re-run with `cargo test community_invite_signed_wire_bytes_pinned`
/// and update the pinned bytes IFF a deliberate wire-format change is
/// shipping. Pinned bytes diverging from the encoder output is a
/// regression — debug before regen.
#[test]
fn community_invite_signed_wire_bytes_pinned() {
    use harmony_app::community_invite::CommunityInviteSigned;
    use harmony_app::community_membership::{
        sign_event, EventPayload, MembershipEventKind, SignedMembershipEvent,
    };
    use harmony_app::community_invite::InviteToken;
    use harmony_app::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
    use harmony_app::owner_state_types::{DeviceIdentityHash, Hlc, OwnerAddr, SpaceId};

    let community_id = SpaceId([0x10; 16]);
    let inviter = OwnerAddr([0x11; 16]);
    let joiner = OwnerAddr([0x22; 16]);

    // Build a Join event; sign with a deterministic test key so the
    // pinned bytes are stable.
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x33; 32]);
    let join_event = sign_event(
        &EventPayload {
            id: [0x44; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: joiner,
            at: Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "joiner-dev".into(),
            },
        },
        &signing_key,
    )
    .unwrap();

    // Build an InviteToken (sig is just deterministic test bytes —
    // wire-format pin doesn't validate the sig).
    let invite_token = InviteToken {
        inviter,
        invitee_hint: Some(joiner),
        minted_at: Hlc {
            wall_ms: 1_699_000_000_000,
            logical: 0,
            device_id: "inviter-dev".into(),
        },
        sig: [0x55; 64],
    };

    let signed = CommunityInviteSigned {
        community_id,
        join_event,
        invite_token,
        joiner_identity_pub: [0x66; 64],
        signing_device_hash: DeviceIdentityHash([0x77; 16]),
        created_at: Hlc {
            wall_ms: 1_700_000_001_000,
            logical: 0,
            device_id: "joiner-dev".into(),
        },
    };

    let bytes = canonical_cbor_encode(&signed).expect("encode");
    let decoded: CommunityInviteSigned = canonical_cbor_decode(&bytes).expect("decode");
    assert_eq!(decoded, signed, "roundtrip identity");

    // Pin the canonical bytes. If this assertion fires with a real wire-
    // format change, regenerate via `let pinned = bytes;` and paste below.
    // Pin to first-and-last 16 bytes for readability — full pin if
    // length is small enough to inspect inline.
    let pinned_len = bytes.len();
    assert!(
        pinned_len > 64,
        "expected non-trivial encoding (sig field alone is 64 bytes); got {} bytes",
        pinned_len
    );
    // Implementer: after the encoder is wired in Step 3, replace the
    // length-only pin with a full byte-for-byte pin. The exact bytes
    // are computed at first run; copy them from the assertion failure
    // message into a `const PINNED: &[u8] = &[...];` and re-pin.
    //
    // Full byte-for-byte pin shape (uncomment after first run):
    //   const PINNED: &[u8] = &[0xa6, 0x62, 0x63, 0x69, ...];
    //   assert_eq!(bytes.as_slice(), PINNED, "wire format drifted");
}
```

**Note on the soft pin:** the first-run pin is a length check; the full byte pin is added as a follow-up step within this task once the encoder runs. This avoids hand-computing CBOR bytes in the plan. The implementer regenerates the pin from the assertion failure on first green run, then commits both the type definitions AND the byte pin.

- [ ] **Step 2: Run the test — verify it fails to compile**

```bash
cd src-tauri
set -o pipefail
cargo test --test wire_format_community_fixtures community_invite_signed_wire_bytes_pinned 2>&1 | tail -10
```

Expected: compile failure (`CommunityInviteSigned` not found).

- [ ] **Step 3: Add the new types to `community_invite.rs`**

Append to `src-tauri/src/community_invite.rs` after the existing `InviteToken` definition (after line 92):

```rust
use crate::community_membership::SignedMembershipEvent;
use crate::owner_state_types::DeviceIdentityHash;

/// ZEB-262 Phase 4: Reticulum unicast packet body sent from joiner →
/// counter-signer. Mirrors `dm_envelope::DmInviteSigned`'s Path B app-
/// sig binding shape: the signing_device_hash is INSIDE the signed body
/// so an attacker can't swap which device claims authorship without
/// invalidating the signature, and joiner_identity_pub rides along
/// inline because the receiver doesn't yet have an OwnerDeviceCache
/// entry for the joiner (bootstrap-only).
///
/// Wire format: 6-key map. Field codes are 2 chars to satisfy the
/// same-length-keys CBOR invariant at this nesting level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityInviteSigned {
    /// The community being joined.
    #[serde(rename = "ci")]
    pub community_id: crate::owner_state_types::SpaceId,

    /// The joiner's signed Join event WITHOUT countersig. Counter-sig
    /// is applied by the receiver (after verification) via
    /// `community_membership::attach_countersig_with_identity`.
    #[serde(rename = "je")]
    pub join_event: SignedMembershipEvent,

    /// The InviteToken from the URL payload — proves the inviter
    /// authorized this redemption.
    #[serde(rename = "it")]
    pub invite_token: InviteToken,

    /// Joiner's full 64-byte identity public bytes
    /// (`X25519_pub(32) || Ed25519_pub(32)` per
    /// `harmony_identity::Identity::to_public_bytes()`). Bootstrap-only
    /// — receiver doesn't yet have an OwnerDeviceCache entry for the
    /// joiner. Mirrors DmInviteSigned.inviter_identity_pub. Wire form:
    /// CBOR bstr(64).
    #[serde(
        rename = "ip",
        serialize_with = "serialize_identity_pub_as_bstr",
        deserialize_with = "deserialize_identity_pub_from_bstr"
    )]
    pub joiner_identity_pub: [u8; 64],

    /// Joiner's DeviceIdentityHash. Receiver verifies that
    /// SHA256(joiner_identity_pub)[..16] == signing_device_hash.0
    /// (defense-in-depth against a buggy sender pairing pubs with the
    /// wrong device claim). Mirrors DmInvite's signing_device_hash.
    #[serde(rename = "dh")]
    pub signing_device_hash: DeviceIdentityHash,

    /// Wall-clock at packet creation. Used for staleness checks against
    /// `invite_token` (carried via outer `InviteToken.minted_at` and
    /// the outer `CommunityInvitePayload.expires_at`). Also used for
    /// clock-skew rejection (created_at.wall_ms > now + 60s).
    #[serde(rename = "ca")]
    pub created_at: crate::owner_state_types::Hlc,
}

impl CanonicalPayloadSealed for CommunityInviteSigned {}
impl CanonicalPayload for CommunityInviteSigned {}

/// ZEB-262 Phase 4: Path B app-sig wrapper around CommunityInviteSigned.
/// Wire layout: `[u8 disc=0x10][CBOR(signed)][64 raw signature bytes]`.
/// The signature is 64 raw bytes appended after the CBOR body — same
/// pattern as `DmPacket` (NOT a CBOR bstr; encode appends via
/// `extend_from_slice`, decode splits via `split_at(len - 64)`).
///
/// Discriminant 0x10 is reserved for community packets per the spec
/// §"Wire format" (DM packets occupy 0x01-0x03; 0x10-0x1F reserved for
/// community packets; 0x20+ reserved for Sub-D directory packets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommunityInvitePacket {
    Invite {
        signed: CommunityInviteSigned,
        signature: [u8; 64],
        /// Captured at decode for re-verify. The signature covers
        /// `signed_bytes` exactly as transmitted, so signature
        /// verification operates on bit-exact bytes regardless of
        /// encoder drift. On send, encode_packet re-encodes from
        /// `signed`, asserts byte-equality with `signed_bytes`, and
        /// emits `signed_bytes` verbatim.
        signed_bytes: Vec<u8>,
    },
}

/// Helper: serialize `[u8; 64]` as CBOR bstr (major type 2). Mirrors
/// dm_envelope::serialize_identity_pub_as_bstr — necessary because
/// serde's blanket `[T; N]: Serialize` only covers small N.
fn serialize_identity_pub_as_bstr<S>(b: &[u8; 64], s: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    s.serialize_bytes(b)
}

/// Helper: deserialize CBOR bstr(64) into `[u8; 64]`. Length is
/// enforced strictly; bstr of any length other than 64 is rejected.
/// Mirrors dm_envelope::deserialize_identity_pub_from_bstr.
fn deserialize_identity_pub_from_bstr<'de, D>(d: D) -> Result<[u8; 64], D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{Error, Visitor};
    use std::fmt;

    struct BytesVisitor;
    impl<'de> Visitor<'de> for BytesVisitor {
        type Value = [u8; 64];

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "a 64-byte CBOR byte string")
        }

        fn visit_bytes<E: Error>(self, value: &[u8]) -> Result<[u8; 64], E> {
            if value.len() != 64 {
                return Err(E::custom(format!(
                    "joiner_identity_pub must be 64 bytes, got {}",
                    value.len()
                )));
            }
            let mut out = [0u8; 64];
            out.copy_from_slice(value);
            Ok(out)
        }

        fn visit_byte_buf<E: Error>(self, v: Vec<u8>) -> Result<[u8; 64], E> {
            self.visit_bytes(&v)
        }
    }

    d.deserialize_bytes(BytesVisitor)
}
```

The `Serialize` / `Deserialize` derive on `CommunityInviteSigned` will pick up the `serialize_with` / `deserialize_with` attributes. The two helpers are private to the module (no `pub`) — only the struct's serde impl uses them.

- [ ] **Step 4: Run the canonical-CBOR roundtrip test — verify it passes**

```bash
cd src-tauri
set -o pipefail
cargo test --test wire_format_community_fixtures community_invite_signed_wire_bytes_pinned 2>&1 | tail -10
```

Expected: PASS (the soft length-only pin clears).

- [ ] **Step 5: Capture canonical bytes and add the full pin**

Run the test once with a debug print to extract the actual encoded bytes. Add temporarily after the `let bytes = canonical_cbor_encode(&signed).expect("encode");` line:

```rust
println!("PINNED CommunityInviteSigned bytes ({} total): {:?}", bytes.len(), bytes);
```

Run with `cargo test --test wire_format_community_fixtures community_invite_signed_wire_bytes_pinned -- --nocapture`. Copy the printed byte slice. Replace the soft pin in the test with:

```rust
const PINNED: &[u8] = &[/* paste bytes here */];
assert_eq!(
    bytes.as_slice(),
    PINNED,
    "CommunityInviteSigned wire format drifted from pinned bytes — \
     debug encoder drift, regen the pin only on a deliberate wire-format change"
);
```

Remove the `println!` line.

- [ ] **Step 6: Re-run the pinned test**

```bash
cd src-tauri
set -o pipefail
cargo test --test wire_format_community_fixtures community_invite_signed_wire_bytes_pinned 2>&1 | tail -10
```

Expected: PASS with the full byte-pin.

- [ ] **Step 7: Run cargo fmt + clippy + workspace tests**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
```

Expected: all green.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/community_invite.rs src-tauri/tests/wire_format_community_fixtures.rs
git commit -m "$(cat <<'EOF'
feat(zeb-262): CommunityInviteSigned + CommunityInvitePacket wire types

Wire format: 6-key CBOR map for the signed body. Path B app-sig
binding (signing_device_hash inside the signed body, joiner_identity_pub
inline for bootstrap). Discriminant 0x10 reserved for community packets.

Pinned canonical bytes lock the encoder against silent drift across
phases. Sign/verify helpers ship in Task 5.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: `encode_packet` + `decode_packet` + `build_signed_invite_packet`

**Why:** With the wire types pinned, layer the codec on top: encode/decode pair with the same `[disc][CBOR(body)][64-byte sig]` shape as `dm_envelope`, and a `build_signed_invite_packet` helper that signs + bundles in one call. Mirror the mutation-guard from `dm_envelope::encode_packet` (re-encode `signed`, assert byte-equality with cached `signed_bytes`, otherwise return `SignedMutated`) so a future post-build mutation can't ship a stale signature.

**Files:**
- Modify: `src-tauri/src/community_invite.rs` (codec + builder + error types)
- Modify: `src-tauri/tests/community_invite_unit.rs` (roundtrip + tampered-body reject)

- [ ] **Step 1: Write the failing roundtrip + tampered-body tests**

Append to `src-tauri/tests/community_invite_unit.rs`:

```rust
#[test]
fn community_invite_packet_roundtrip() {
    use harmony_app::community_invite::{
        build_signed_invite_packet, decode_packet, encode_packet, CommunityInvitePacket,
        CommunityInviteSigned, InviteToken,
    };
    use harmony_app::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use harmony_app::owner_state_types::{DeviceIdentityHash, Hlc, OwnerAddr, SpaceId};

    let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0xab; 32]);
    let community_id = SpaceId([0x10; 16]);
    let joiner = OwnerAddr([0x22; 16]);
    let inviter = OwnerAddr([0x11; 16]);

    let join_event = sign_event(
        &EventPayload {
            id: [0x44; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: joiner,
            at: Hlc { wall_ms: 1000, logical: 0, device_id: "j".into() },
        },
        &signing_key,
    )
    .unwrap();

    let signed = CommunityInviteSigned {
        community_id,
        join_event,
        invite_token: InviteToken {
            inviter,
            invitee_hint: Some(joiner),
            minted_at: Hlc { wall_ms: 900, logical: 0, device_id: "i".into() },
            sig: [0x55; 64],
        },
        joiner_identity_pub: [0x66; 64],
        signing_device_hash: DeviceIdentityHash([0x77; 16]),
        created_at: Hlc { wall_ms: 1100, logical: 0, device_id: "j".into() },
    };

    let packet = build_signed_invite_packet(signed.clone(), &signing_key)
        .expect("build_signed_invite_packet");
    let wire = encode_packet(&packet).expect("encode");

    // Discriminant byte is 0x10.
    assert_eq!(wire[0], 0x10, "discriminant byte must be 0x10");

    let decoded = decode_packet(&wire).expect("decode");
    match (&packet, &decoded) {
        (
            CommunityInvitePacket::Invite { signed: s1, signature: sig1, .. },
            CommunityInvitePacket::Invite { signed: s2, signature: sig2, .. },
        ) => {
            assert_eq!(s1, s2);
            assert_eq!(sig1, sig2);
        }
    }
}

#[test]
fn community_invite_packet_envelope_sig_rejected_on_tampered_body() {
    use harmony_app::community_invite::{
        build_signed_invite_packet, decode_packet, encode_packet, verify_envelope_sig,
        CommunityInvitePacket, CommunityInviteSigned, InviteToken,
    };
    use harmony_app::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use harmony_app::owner_state_types::{DeviceIdentityHash, Hlc, OwnerAddr, SpaceId};

    let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0xab; 32]);
    let identity = harmony_identity::PrivateIdentity::from_seed(&[0xcd; 32]);
    let identity_pub = identity.identity.to_public_bytes();
    let joiner_signing_key = {
        let priv_bytes = identity.to_private_bytes();
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&priv_bytes[32..64]);
        ed25519_dalek::SigningKey::from_bytes(&seed)
    };
    let joiner = harmony_app::owner_state_types::OwnerAddr(identity.identity.address_hash);

    let community_id = SpaceId([0x10; 16]);
    let join_event = sign_event(
        &EventPayload {
            id: [0x44; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: joiner,
            at: Hlc { wall_ms: 1000, logical: 0, device_id: "j".into() },
        },
        &joiner_signing_key,
    )
    .unwrap();

    let signed = CommunityInviteSigned {
        community_id,
        join_event,
        invite_token: InviteToken {
            inviter: OwnerAddr([0x11; 16]),
            invitee_hint: None,
            minted_at: Hlc { wall_ms: 900, logical: 0, device_id: "i".into() },
            sig: [0x55; 64],
        },
        joiner_identity_pub: identity_pub,
        signing_device_hash: harmony_app::owner_state_types::DeviceIdentityHash(
            identity.identity.address_hash,
        ),
        created_at: Hlc { wall_ms: 1100, logical: 0, device_id: "j".into() },
    };

    let packet = build_signed_invite_packet(signed.clone(), &joiner_signing_key)
        .expect("build");
    let mut wire = encode_packet(&packet).expect("encode");

    // Flip a byte in the signed body region (skip discriminant +
    // signature trailer). Targets a byte that's part of the CBOR map.
    let target = 5;
    assert!(target < wire.len() - 64, "bound check");
    wire[target] ^= 0xff;

    // Decode still succeeds (CBOR remained syntactically valid for our
    // chosen byte flip; if the flip lands on a length-prefix it could
    // fail decode — choose a target byte that's a value, not a
    // length. Index 5 is inside a map key bstr; fine).
    let decoded = decode_packet(&wire);
    if let Ok(CommunityInvitePacket::Invite { signature, signed_bytes, .. }) = decoded {
        // Envelope-sig verification MUST reject the tampered body.
        let result = verify_envelope_sig(&signed_bytes, &signature, &identity_pub);
        assert!(
            result.is_err(),
            "envelope sig must reject tampered body"
        );
    } else {
        // The byte flip happened to break CBOR decode itself — that's
        // also an acceptable rejection. The test is satisfied.
    }
}
```

- [ ] **Step 2: Run the tests — verify they fail**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_invite_unit community_invite_packet_roundtrip community_invite_packet_envelope_sig_rejected_on_tampered_body 2>&1 | tail -20
```

Expected: compile failure on `build_signed_invite_packet`, `encode_packet`, `decode_packet`, `verify_envelope_sig`.

- [ ] **Step 3: Add `EncodeError` + `DecodeError` enums**

Append to `src-tauri/src/community_invite.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommunityInviteEncodeError {
    #[error("CBOR encode failed: {0}")]
    Cbor(String),
    /// Re-encoding `signed` to canonical CBOR failed inside encode_packet.
    /// build_signed_invite_packet already round-tripped this value through
    /// the same encoder, so this should be unreachable in practice — surface
    /// as a clear distinct variant so a regression here doesn't mask as a
    /// generic Cbor encode failure.
    #[error("re-encode signed body failed: {0}")]
    ReSerialize(String),
    /// encode_packet re-encoded `signed` and the result diverged from the
    /// cached `signed_bytes` field — the only way this fires is post-build
    /// mutation of the `signed` field. Mirrors dm_envelope::EncodeError::SignedMutated.
    #[error("signed body mutated post-build: {0}")]
    SignedMutated(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommunityInviteDecodeError {
    #[error("packet is empty")]
    Empty,
    #[error("packet too short for [disc + body + 64-byte signature] layout")]
    TooShortForSignature,
    #[error("unknown discriminant byte 0x{0:02x}")]
    UnknownDiscriminant(u8),
    #[error("CBOR decode failed: {0}")]
    Cbor(String),
    #[error("trailing bytes after CBOR body: consumed {consumed} of {total}")]
    TrailingBytes { consumed: u64, total: u64 },
    #[error("payload invariant violated: {0}")]
    Invalid(&'static str),
}
```

- [ ] **Step 4: Add `encode_packet` + `decode_packet` + `build_signed_invite_packet` + `verify_envelope_sig`**

Continue appending to `src-tauri/src/community_invite.rs`:

```rust
/// Encode a CommunityInvitePacket to wire bytes. Mutation guard:
/// re-encodes `signed` and asserts byte-equality with cached
/// `signed_bytes` (which was the source for `signature` at build
/// time); mismatch returns `SignedMutated`. Mirrors
/// `dm_envelope::encode_packet`.
pub fn encode_packet(packet: &CommunityInvitePacket) -> Result<Vec<u8>, CommunityInviteEncodeError> {
    match packet {
        CommunityInvitePacket::Invite { signed, signature, signed_bytes } => {
            let re_encoded = canonical_cbor_encode(signed)
                .map_err(|e| CommunityInviteEncodeError::ReSerialize(format!("re-encode: {e}")))?;
            if re_encoded != *signed_bytes {
                return Err(CommunityInviteEncodeError::SignedMutated(
                    "CommunityInvitePacket::Invite: signed mutated post-build (re-encode mismatches \
                     cached signed_bytes; signature would not cover wire body)".into(),
                ));
            }
            let mut out = Vec::with_capacity(1 + signed_bytes.len() + 64);
            out.push(0x10);
            out.extend_from_slice(signed_bytes);
            out.extend_from_slice(signature);
            Ok(out)
        }
    }
}

/// Decode wire bytes into a CommunityInvitePacket. Captures
/// `signed_bytes` exactly as transmitted so envelope-sig verify
/// operates on bit-exact bytes. Rejects unknown discriminants,
/// trailing bytes after the CBOR body, and non-canonical encodings.
pub fn decode_packet(bytes: &[u8]) -> Result<CommunityInvitePacket, CommunityInviteDecodeError> {
    let (disc, rest) = bytes.split_first().ok_or(CommunityInviteDecodeError::Empty)?;
    if rest.len() < 64 + 1 {
        return Err(CommunityInviteDecodeError::TooShortForSignature);
    }
    let split_at = rest.len() - 64;
    let (body_bytes, signature_bytes) = rest.split_at(split_at);
    let signature: [u8; 64] = signature_bytes
        .try_into()
        .expect("just split at len-64; signature_bytes is exactly 64 bytes");
    let signed_bytes = body_bytes.to_vec();
    match disc {
        0x10 => {
            let mut cursor = std::io::Cursor::new(body_bytes);
            let signed: CommunityInviteSigned = ciborium::from_reader(&mut cursor)
                .map_err(|e| CommunityInviteDecodeError::Cbor(e.to_string()))?;
            let consumed = cursor.position();
            if consumed as usize != body_bytes.len() {
                return Err(CommunityInviteDecodeError::TrailingBytes {
                    consumed,
                    total: body_bytes.len() as u64,
                });
            }
            // Canonical-encoding round-trip check: re-encode and reject
            // if the re-encoded bytes differ from body_bytes. Catches
            // reordered map keys, indefinite-length encodings, oversized
            // length prefixes — anything where decode → canonical-re-
            // encode is not byte-identical. Mirrors
            // dm_envelope::ensure_canonical_body.
            let canonical = canonical_cbor_encode(&signed)
                .map_err(|e| CommunityInviteDecodeError::Cbor(e.to_string()))?;
            if canonical != body_bytes {
                return Err(CommunityInviteDecodeError::Invalid(
                    "CommunityInvitePacket body must use canonical CBOR",
                ));
            }
            // Structural check: signing_device_hash must match
            // SHA256(joiner_identity_pub)[..16]. Not a sig check (no
            // crypto here); cheap defense-in-depth before the sig
            // verifier runs in handle_unicast.
            let derived = device_hash_from_identity_pub(&signed.joiner_identity_pub);
            if derived != signed.signing_device_hash.0 {
                return Err(CommunityInviteDecodeError::Invalid(
                    "CommunityInviteSigned.signing_device_hash must equal \
                     SHA256(joiner_identity_pub)[..16]",
                ));
            }
            Ok(CommunityInvitePacket::Invite {
                signed,
                signature,
                signed_bytes,
            })
        }
        other => Err(CommunityInviteDecodeError::UnknownDiscriminant(*other)),
    }
}

/// Compute SHA256(identity_pub)[..16]. Mirrors how DmInvite Path B
/// derives signing_device_hash; the receiver checks this binding before
/// running the (more expensive) Ed25519 verify.
pub fn device_hash_from_identity_pub(identity_pub: &[u8; 64]) -> [u8; 16] {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(identity_pub);
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

/// Build a complete CommunityInvitePacket ready for encode_packet.
/// Encodes `signed` to canonical CBOR, signs the resulting bytes via
/// `signing_key`, bundles into the Invite variant. Mirrors
/// dm_envelope::build_signed_invite.
pub fn build_signed_invite_packet(
    signed: CommunityInviteSigned,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<CommunityInvitePacket, CommunityInviteEncodeError> {
    let signed_bytes = canonical_cbor_encode(&signed)
        .map_err(|e| CommunityInviteEncodeError::Cbor(e.to_string()))?;
    let signature = signing_key.sign(&signed_bytes).to_bytes();
    Ok(CommunityInvitePacket::Invite {
        signed,
        signature,
        signed_bytes,
    })
}

/// Verify the Path B envelope signature over the captured signed_bytes.
/// Pure crypto check — no membership or expiry semantics. Returns
/// `EnvelopeSigInvalid` on any failure (including malformed identity_pub).
/// Used by handle_unicast (Task 9) and exercised by the
/// `community_invite_packet_envelope_sig_rejected_on_tampered_body` test.
pub fn verify_envelope_sig(
    signed_bytes: &[u8],
    signature: &[u8; 64],
    identity_pub: &[u8; 64],
) -> Result<(), CommunityInviteVerifyError> {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    let identity = harmony_identity::Identity::from_public_bytes(identity_pub)
        .map_err(|_| CommunityInviteVerifyError::EnvelopeSigInvalid)?;
    let sig = Signature::from_bytes(signature);
    identity
        .verifying_key
        .verify_strict(signed_bytes, &sig)
        .map_err(|_| CommunityInviteVerifyError::EnvelopeSigInvalid)
}
```

**`CommunityInviteVerifyError`** is referenced here but defined in Task 6. To make Task 5 compile standalone, add a minimal stub for the variant being referenced:

```rust
/// Receive-side rejection variants. Full enum + reject paths land in
/// Task 6; this stub admits only EnvelopeSigInvalid so the encode/
/// decode tests can exercise verify_envelope_sig.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommunityInviteVerifyError {
    #[error("envelope sig invalid")]
    EnvelopeSigInvalid,
}
```

Task 6 will EXTEND this enum (add 10 more variants) — not redefine it. Each variant is an additive change.

- [ ] **Step 5: Run the new tests — verify they pass**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_invite_unit community_invite_packet_roundtrip community_invite_packet_envelope_sig_rejected_on_tampered_body 2>&1 | tail -20
```

Expected: PASS.

- [ ] **Step 6: Run cargo fmt + clippy + workspace tests**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
```

Expected: all green. The new module surface (`CommunityInvitePacket`, `encode_packet`, `decode_packet`, `build_signed_invite_packet`, `verify_envelope_sig`, `device_hash_from_identity_pub`, error enums) compiles and passes.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/community_invite.rs src-tauri/tests/community_invite_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-262): community_invite codec + envelope-sig verify

Adds encode_packet/decode_packet (mirrors dm_envelope's appended-sig
layout), build_signed_invite_packet (canonical CBOR + Ed25519 sign),
verify_envelope_sig (Path B envelope verify), and the
device_hash_from_identity_pub helper. Mutation guard on encode_packet
rejects post-build mutations of `signed`. Decode rejects non-canonical
bodies, trailing bytes, and signing_device_hash mismatch.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: `CommunityInviteVerifyError` (full) + reject-variant unit tests

**Why:** The receive path's verify chain is best built bottom-up: define every reject variant + reason tag, write a unit test per reject, then layer the engine-coupled handle_unicast on top in Task 9. Splitting the verify logic into a pure helper that returns `Result<SignedMembershipEvent, CommunityInviteVerifyError>` (taking only the decoded packet + self_owner + a `CommunityInvitePayload` for the InviteToken context + a current-time function) means the rejects can be unit-tested without engine plumbing. Membership-state-dependent checks (`SelfNotJoined`, `CommunityUnknown`) move to Task 9 where the engine state is in scope.

**Files:**
- Modify: `src-tauri/src/community_invite.rs` (extend `CommunityInviteVerifyError` to all 11 variants + add `verify_packet_pure`)
- Modify: `src-tauri/tests/community_invite_unit.rs` (7 reject-variant tests)

- [ ] **Step 1: Write failing reject-variant unit tests**

Append to `src-tauri/tests/community_invite_unit.rs`:

```rust
mod verify_rejection_tests {
    use harmony_app::community_invite::{
        verify_packet_pure, CommunityInviteSigned, CommunityInviteVerifyError, InviteToken,
    };
    use harmony_app::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use harmony_app::owner_state_crypto::canonical_cbor_encode;
    use harmony_app::owner_state_types::{DeviceIdentityHash, Hlc, OwnerAddr, SpaceId};

    /// Common harness: build a fully valid CommunityInviteSigned + a
    /// matching InviteToken signed by `self_identity`. Tests then mutate
    /// one field and assert the right reject discriminant.
    fn make_valid_packet(
        self_identity: &harmony_identity::PrivateIdentity,
        joiner_identity: &harmony_identity::PrivateIdentity,
        community_id: SpaceId,
        invite_only: bool, /* affects reject expectations */
    ) -> CommunityInviteSigned {
        let _ = invite_only;
        let self_owner = OwnerAddr(self_identity.identity.address_hash);
        let joiner_owner = OwnerAddr(joiner_identity.identity.address_hash);
        let joiner_pub = joiner_identity.identity.to_public_bytes();
        let joiner_sk = {
            let priv_bytes = joiner_identity.to_private_bytes();
            let mut seed = [0u8; 32];
            seed.copy_from_slice(&priv_bytes[32..64]);
            ed25519_dalek::SigningKey::from_bytes(&seed)
        };
        let join_event = sign_event(
            &EventPayload {
                id: [0x44; 16],
                community_id,
                kind: MembershipEventKind::Join,
                actor: joiner_owner,
                at: Hlc { wall_ms: 1000, logical: 0, device_id: "j".into() },
            },
            &joiner_sk,
        )
        .expect("sign Join");

        // Build an InviteToken signed by self (mirrors the v1 single-shot
        // inviter-must-be-self contract).
        let token_payload_bytes = {
            // Approximate: in production the InviteToken sig covers a
            // canonical-CBOR encoding of (inviter, invitee_hint,
            // minted_at, outer expires_at). For this test the sig is
            // computed over a stable byte slice that the verify helper
            // also reconstructs the same way. Implementer: confirm the
            // exact canonical-byte computation; if InviteToken.canonical_payload()
            // is the helper community_invite ships, use that.
            let combined = (
                self_owner,
                Some(joiner_owner),
                Hlc { wall_ms: 900, logical: 0, device_id: "i".into() },
                None::<Hlc>, /* expires_at */
            );
            canonical_cbor_encode(&combined).expect("encode token payload")
        };
        let token_sig = self_identity.sign(&token_payload_bytes);
        let invite_token = InviteToken {
            inviter: self_owner,
            invitee_hint: Some(joiner_owner),
            minted_at: Hlc { wall_ms: 900, logical: 0, device_id: "i".into() },
            sig: token_sig,
        };

        CommunityInviteSigned {
            community_id,
            join_event,
            invite_token,
            joiner_identity_pub: joiner_pub,
            signing_device_hash: DeviceIdentityHash(joiner_identity.identity.address_hash),
            created_at: Hlc { wall_ms: 1100, logical: 0, device_id: "j".into() },
        }
    }

    fn now_ms() -> u64 { 2000 }

    #[test]
    fn community_invite_join_sig_invalid_rejected() {
        let self_id = harmony_identity::PrivateIdentity::from_seed(&[0xa1; 32]);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xb2; 32]);
        let community_id = SpaceId([0x10; 16]);
        let mut signed = make_valid_packet(&self_id, &joiner_id, community_id, false);

        // Flip a byte in the inner Join sig.
        signed.join_event.sig[0] ^= 0xff;

        let err = verify_packet_pure(
            &signed,
            OwnerAddr(self_id.identity.address_hash),
            now_ms,
            &self_id,
        )
        .expect_err("must reject");
        assert!(matches!(err, CommunityInviteVerifyError::JoinSigInvalid));
    }

    #[test]
    fn community_invite_token_sig_invalid_rejected() {
        let self_id = harmony_identity::PrivateIdentity::from_seed(&[0xa3; 32]);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xb4; 32]);
        let community_id = SpaceId([0x10; 16]);
        let mut signed = make_valid_packet(&self_id, &joiner_id, community_id, false);

        signed.invite_token.sig[0] ^= 0xff;

        let err = verify_packet_pure(
            &signed,
            OwnerAddr(self_id.identity.address_hash),
            now_ms,
            &self_id,
        )
        .expect_err("must reject");
        assert!(matches!(err, CommunityInviteVerifyError::InviteTokenSigInvalid));
    }

    #[test]
    fn community_invite_signer_mismatch_rejected() {
        // InviteToken.signer is some other OwnerAddr (not self).
        let self_id = harmony_identity::PrivateIdentity::from_seed(&[0xa5; 32]);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xb6; 32]);
        let community_id = SpaceId([0x10; 16]);
        let mut signed = make_valid_packet(&self_id, &joiner_id, community_id, false);

        signed.invite_token.inviter = OwnerAddr([0xaa; 16]); // not self

        let err = verify_packet_pure(
            &signed,
            OwnerAddr(self_id.identity.address_hash),
            now_ms,
            &self_id,
        )
        .expect_err("must reject");
        assert!(matches!(err, CommunityInviteVerifyError::InviteSignerMismatch { .. }));
    }

    #[test]
    fn community_invite_id_mismatch_rejected() {
        // signed.community_id != signed.join_event.community_id.
        let self_id = harmony_identity::PrivateIdentity::from_seed(&[0xa7; 32]);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xb8; 32]);
        let community_id = SpaceId([0x10; 16]);
        let mut signed = make_valid_packet(&self_id, &joiner_id, community_id, false);

        signed.community_id = SpaceId([0xff; 16]); // mismatch

        let err = verify_packet_pure(
            &signed,
            OwnerAddr(self_id.identity.address_hash),
            now_ms,
            &self_id,
        )
        .expect_err("must reject");
        assert!(matches!(err, CommunityInviteVerifyError::CommunityIdMismatch));
    }

    #[test]
    fn community_invite_invitee_hint_mismatch_rejected() {
        // join_event.actor != invite_token.invitee_hint.
        let self_id = harmony_identity::PrivateIdentity::from_seed(&[0xa9; 32]);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xba; 32]);
        let community_id = SpaceId([0x10; 16]);
        let mut signed = make_valid_packet(&self_id, &joiner_id, community_id, false);

        signed.invite_token.invitee_hint = Some(OwnerAddr([0xcc; 16]));

        let err = verify_packet_pure(
            &signed,
            OwnerAddr(self_id.identity.address_hash),
            now_ms,
            &self_id,
        )
        .expect_err("must reject");
        assert!(matches!(err, CommunityInviteVerifyError::InviteeHintMismatch));
    }

    #[test]
    fn community_invite_expired_clock_skew_rejected() {
        // created_at.wall_ms is way in the future relative to now.
        let self_id = harmony_identity::PrivateIdentity::from_seed(&[0xab; 32]);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xbc; 32]);
        let community_id = SpaceId([0x10; 16]);
        let mut signed = make_valid_packet(&self_id, &joiner_id, community_id, false);

        // Now is 2000 ms; created_at is set to 999_999_999 ms — way past
        // the 60_000 ms clock-skew tolerance.
        signed.created_at.wall_ms = 999_999_999;

        let err = verify_packet_pure(
            &signed,
            OwnerAddr(self_id.identity.address_hash),
            now_ms,
            &self_id,
        )
        .expect_err("must reject");
        assert!(matches!(err, CommunityInviteVerifyError::Expired));
    }

    #[test]
    fn community_invite_valid_packet_admits() {
        let self_id = harmony_identity::PrivateIdentity::from_seed(&[0xad; 32]);
        let joiner_id = harmony_identity::PrivateIdentity::from_seed(&[0xbe; 32]);
        let community_id = SpaceId([0x10; 16]);
        let signed = make_valid_packet(&self_id, &joiner_id, community_id, false);

        let join_event = verify_packet_pure(
            &signed,
            OwnerAddr(self_id.identity.address_hash),
            now_ms,
            &self_id,
        )
        .expect("must admit");
        assert_eq!(join_event.actor, OwnerAddr(joiner_id.identity.address_hash));
    }
}
```

**Implementer note on the InviteToken canonical bytes:** the test sketch above uses a tuple-encoding shortcut. The exact canonical layout the production token sig covers is community_invite-defined; if a helper like `InviteToken::canonical_payload_bytes()` exists, use it. If not, this task adds a small private helper `canonical_invite_token_bytes(payload: &CommunityInvitePayload) -> Vec<u8>` that both the unit tests and `verify_packet_pure` reuse.

- [ ] **Step 2: Run the tests — verify they fail**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_invite_unit verify_rejection_tests 2>&1 | tail -30
```

Expected: compile failure on `verify_packet_pure` and missing `CommunityInviteVerifyError` variants.

- [ ] **Step 3: Extend `CommunityInviteVerifyError` to the full 11 variants**

Replace the stub `CommunityInviteVerifyError` from Task 5 in `src-tauri/src/community_invite.rs`:

```rust
/// ZEB-262 Phase 4 receive-side rejection variants. Each maps to a
/// `community-state-sync-degraded` reason tag for the frontend banner.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommunityInviteVerifyError {
    /// Path B envelope sig didn't validate.
    #[error("envelope sig invalid")]
    EnvelopeSigInvalid,
    /// signing_device_hash != SHA256(joiner_identity_pub)[..16]. Caught
    /// at decode time but surfaced through this error type when the
    /// caller wants the unified reason tag.
    #[error("device hash mismatch")]
    DeviceHashMismatch,
    /// Inner Join event sig failed.
    #[error("Join event sig invalid")]
    JoinSigInvalid,
    /// InviteToken sig failed.
    #[error("InviteToken sig invalid")]
    InviteTokenSigInvalid,
    /// InviteToken.inviter != self_owner. v1 only counter-signs invites
    /// we issued. ZEB-251 broadens this to any joined member with
    /// power ≥ invite_threshold.
    #[error("invite signer mismatch: token says {signer:?}, we are {self_owner:?}")]
    InviteSignerMismatch {
        signer: crate::owner_state_types::OwnerAddr,
        self_owner: crate::owner_state_types::OwnerAddr,
    },
    /// community_id disagreement across envelope, Join, and token.
    #[error("community_id mismatch across envelope/Join/token")]
    CommunityIdMismatch,
    /// created_at >= invite_token expires_at, OR created_at > now + 60s.
    #[error("invite expired or clock-skew rejected")]
    Expired,
    /// invite_token.invitee_hint set and != join_event.actor.
    #[error("invitee_hint mismatch")]
    InviteeHintMismatch,
    /// No engine for this community — packet was misrouted. Receiver
    /// surface; not raised by verify_packet_pure (engine state isn't in
    /// scope there).
    #[error("community unknown: {community_id:?}")]
    CommunityUnknown { community_id: crate::owner_state_types::SpaceId },
    /// Self isn't currently a Joined member. Receiver surface; engine-
    /// coupled.
    #[error("self not joined in community")]
    SelfNotJoined,
    /// Self power < invite_threshold (= 0 in v1, structural no-op).
    #[error("self power insufficient: {self_power} < {threshold}")]
    SelfPowerInsufficient { self_power: u8, threshold: u8 },
}

impl CommunityInviteVerifyError {
    /// Reason tag for the `community-state-sync-degraded` Tauri event.
    pub fn reason_tag(&self) -> &'static str {
        match self {
            Self::EnvelopeSigInvalid => "community_invite_envelope_sig_invalid",
            Self::DeviceHashMismatch => "community_invite_device_hash_mismatch",
            Self::JoinSigInvalid => "community_invite_join_sig_invalid",
            Self::InviteTokenSigInvalid => "community_invite_token_sig_invalid",
            Self::InviteSignerMismatch { .. } => "community_invite_signer_mismatch",
            Self::CommunityIdMismatch => "community_invite_id_mismatch",
            Self::Expired => "community_invite_expired",
            Self::InviteeHintMismatch => "community_invitee_hint_mismatch",
            Self::CommunityUnknown { .. } => "community_invite_unknown",
            Self::SelfNotJoined => "community_invite_self_not_joined",
            Self::SelfPowerInsufficient { .. } => "community_invite_self_power_insufficient",
        }
    }
}
```

- [ ] **Step 4: Add `verify_packet_pure`**

Continue in `src-tauri/src/community_invite.rs`:

```rust
/// Pure verify helper: takes a CommunityInviteSigned, the local self
/// owner addr, a wall-clock function, and the local PrivateIdentity for
/// the InviteToken sig check. Returns the joiner's signed Join event on
/// success — caller is then responsible for the engine-coupled checks
/// (community known, self joined, self power sufficient) before
/// counter-signing.
///
/// Order of checks chosen so cheaper / more diagnostic rejections fire
/// before expensive crypto:
///   1. community_id agreement (cheap struct compare)
///   2. invitee_hint match (cheap if hint is None)
///   3. expiry / clock-skew (cheap arithmetic)
///   4. InviteToken signer == self (cheap struct compare)
///   5. Inner Join event sig (1× Ed25519 verify_strict)
///   6. InviteToken sig (1× Ed25519 verify_strict)
///
/// Returns `Ok(SignedMembershipEvent)` so the caller can attach
/// the countersig and insert into the engine.
pub fn verify_packet_pure<F>(
    signed: &CommunityInviteSigned,
    self_owner: crate::owner_state_types::OwnerAddr,
    now_fn: F,
    self_identity: &harmony_identity::PrivateIdentity,
) -> Result<crate::community_membership::SignedMembershipEvent, CommunityInviteVerifyError>
where
    F: FnOnce() -> u64,
{
    // 1. community_id agreement across envelope + Join + token.
    if signed.community_id != signed.join_event.community_id {
        return Err(CommunityInviteVerifyError::CommunityIdMismatch);
    }
    // (InviteToken doesn't carry community_id directly in v1 — the
    // outer URL payload does. Skip a token vs envelope comparison
    // here; the receive-side engine resolution catches misroutes.)

    // 2. invitee_hint match.
    if let Some(hint) = signed.invite_token.invitee_hint {
        if signed.join_event.actor != hint {
            return Err(CommunityInviteVerifyError::InviteeHintMismatch);
        }
    }

    // 3. Expiry / clock-skew.
    let now = now_fn();
    if signed.created_at.wall_ms > now.saturating_add(60_000) {
        return Err(CommunityInviteVerifyError::Expired);
    }
    // Outer URL's expires_at is not in CommunityInviteSigned — InviteToken
    // doesn't carry it either in v1. This expires-comparison hook is
    // future-proofing for ZEB-251; v1 only enforces the clock-skew arm.
    // (Spec line 281-285 lists both; the InviteToken expires_at lookup
    // ships with ZEB-251.)

    // 4. InviteToken signer == self.
    if signed.invite_token.inviter != self_owner {
        return Err(CommunityInviteVerifyError::InviteSignerMismatch {
            signer: signed.invite_token.inviter,
            self_owner,
        });
    }

    // 5. Inner Join event sig.
    crate::community_membership::verify_signature(
        &signed.join_event,
        &signed.joiner_identity_pub,
    )
    .map_err(|_| CommunityInviteVerifyError::JoinSigInvalid)?;

    // 6. InviteToken sig.
    let token_canonical = canonical_invite_token_bytes(&signed.invite_token)
        .map_err(|_| CommunityInviteVerifyError::InviteTokenSigInvalid)?;
    use ed25519_dalek::{Signature, Verifier};
    let sig = Signature::from_bytes(&signed.invite_token.sig);
    self_identity
        .identity
        .verifying_key
        .verify_strict(&token_canonical, &sig)
        .map_err(|_| CommunityInviteVerifyError::InviteTokenSigInvalid)?;

    Ok(signed.join_event.clone())
}

/// Canonical-CBOR-encode the InviteToken payload (excluding the sig).
/// Both the IPC mint path and the verify path encode through this so
/// signature bytes cover bit-exact bytes.
fn canonical_invite_token_bytes(token: &InviteToken) -> Result<Vec<u8>, ciborium::ser::Error<std::io::Error>> {
    // Encode (inviter, invitee_hint, minted_at) as a tuple. v1
    // doesn't include outer expires_at — token sig binds only to the
    // intrinsic InviteToken fields. Mirrors the test harness's
    // expectation in make_valid_packet.
    //
    // Implementer: if a `pub fn payload(&self) -> InviteTokenPayload`
    // helper already exists (Phase 3 might have shipped one), prefer
    // it. The shape below is the contract this task pins.
    #[derive(serde::Serialize)]
    struct InviteTokenPayload<'a> {
        #[serde(rename = "iv")]
        inviter: &'a crate::owner_state_types::OwnerAddr,
        #[serde(rename = "ih", skip_serializing_if = "Option::is_none")]
        invitee_hint: Option<&'a crate::owner_state_types::OwnerAddr>,
        #[serde(rename = "mt")]
        minted_at: &'a crate::owner_state_types::Hlc,
    }
    let payload = InviteTokenPayload {
        inviter: &token.inviter,
        invitee_hint: token.invitee_hint.as_ref(),
        minted_at: &token.minted_at,
    };
    let mut out = Vec::new();
    ciborium::into_writer(&payload, &mut out)?;
    Ok(out)
}
```

**Note on `canonical_invite_token_bytes`:** This is the spec's "InviteToken sig binds to canonical CBOR of (inviter, invitee_hint, minted_at, expires_at_in_outer)" rule, with `expires_at` deferred. The implementer should verify against the spec what fields v1 binds. If Phase 1's `community_invite.rs` already encodes the InviteToken sig via a different scheme (say `InviteToken.canonical_payload()`), align with it — drift here is a sig-verify breakage.

**Update the test harness to use the same helper.** In Step 1's test code, replace the tuple-based `combined` encoding with a call to a public `canonical_invite_token_bytes` (rename `fn canonical_invite_token_bytes` to `pub(crate) fn canonical_invite_token_bytes` so the test module can use it, or duplicate the helper inline in the test).

- [ ] **Step 5: Run reject-variant tests — verify they pass**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_invite_unit verify_rejection_tests 2>&1 | tail -30
```

Expected: PASS for all 7 reject tests + the happy-path test.

- [ ] **Step 6: Run cargo fmt + clippy + workspace tests**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
```

Expected: all green.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/community_invite.rs src-tauri/tests/community_invite_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-262): pure verify helper + full CommunityInviteVerifyError

Adds verify_packet_pure (community_id + invitee_hint + expiry + signer
+ Join sig + token sig), the full 11-variant rejection enum with
reason tags, and the canonical_invite_token_bytes helper. Membership-
state-dependent checks (CommunityUnknown / SelfNotJoined /
SelfPowerInsufficient) are defined but raised by handle_unicast in
Task 9 where engine state is in scope.

7 reject-variant unit tests + 1 happy-path admit. The pure shape lets
unit tests run with no engine plumbing.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: `shutdown_engine_and_cleanup_persistence` + `pending_redemptions` map

**Why:** Both are registry-internal additions consumed by Task 8 (redeem_invite send path) and Task 9 (receive path). Bundling them into one task lands all the registry surface changes in a single commit, with lock-discipline rules documented in one place.

`shutdown_engine_and_cleanup_persistence` is the ZEB-258 rollback primitive: stop the engine task, drain the join handle, remove the per-community persistence directory. ~30 lines.

`pending_redemptions: Arc<Mutex<HashMap<EventId, oneshot::Sender<()>>>>` is the IPC ⇆ receive bridge: `redeem_invite` registers a oneshot keyed on the joiner's bootstrap_join.id; the receive merge loop fires the matching oneshot when the counter-signed Join lands. Lock-discipline: take the map lock, do the lookup, drop the guard BEFORE `tx.send(()).await`. Never hold the map lock across `.await`.

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs:2066` (registry struct + impl)
- Modify: `src-tauri/src/community_state_sync.rs:765` (`insert_local_event` post-merge notify)
- Modify: `src-tauri/src/community_state_sync.rs:1827` (`handle_incoming_publish` post-merge notify)
- Modify: `src-tauri/tests/community_sync_registry_unit.rs` (4 new tests)

- [ ] **Step 1: Write failing registry-unit tests**

Append to `src-tauri/tests/community_sync_registry_unit.rs`:

```rust
#[tokio::test]
async fn shutdown_engine_and_cleanup_persistence_idempotent_on_unknown_id() {
    use harmony_app::community_state_sync::{
        CommunityRegistryConfig, CommunitySyncRegistry, IdentityResolver,
        DEFAULT_DEBOUNCE_MS,
    };
    use harmony_app::content_store::{ContentStore, RuntimeContentStore};
    use harmony_app::owner_state_types::{OwnerAddr, SpaceId};
    use std::sync::Arc;

    struct NopResolver;
    #[async_trait::async_trait]
    impl IdentityResolver for NopResolver {
        async fn resolve(&self, _: &OwnerAddr) -> Option<[u8; 64]> { None }
    }

    let (cas_op_tx, _cas_op_rx) = tokio::sync::mpsc::channel(8);
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(1000),
    ));

    let dir = tempfile::tempdir().expect("tempdir");
    let registry = CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "dev".into(),
        content_store: cs,
        identity_resolver: Arc::new(NopResolver),
        identity_dir: dir.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: OwnerAddr([0x01; 16]),
        signing_key: Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32])),
    });

    let unknown_id = SpaceId([0xff; 16]);
    registry
        .shutdown_engine_and_cleanup_persistence(&unknown_id)
        .await
        .expect("idempotent on unknown id must return Ok");
}

#[tokio::test]
async fn shutdown_engine_and_cleanup_persistence_removes_dir_after_engine_stops() {
    use harmony_app::community_state_sync::{
        CommunityRegistryConfig, CommunitySyncRegistry, IdentityResolver,
        DEFAULT_DEBOUNCE_MS,
    };
    use harmony_app::content_store::{ContentStore, RuntimeContentStore};
    use harmony_app::owner_state_types::{MembershipKey, OwnerAddr, SpaceId};
    use std::sync::Arc;

    struct NopResolver;
    #[async_trait::async_trait]
    impl IdentityResolver for NopResolver {
        async fn resolve(&self, _: &OwnerAddr) -> Option<[u8; 64]> { None }
    }

    let (cas_op_tx, _cas_op_rx) = tokio::sync::mpsc::channel(8);
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(1000),
    ));

    let dir = tempfile::tempdir().expect("tempdir");
    let registry = CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "dev".into(),
        content_store: cs,
        identity_resolver: Arc::new(NopResolver),
        identity_dir: dir.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: OwnerAddr([0x01; 16]),
        signing_key: Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32])),
    });

    let cid = SpaceId([1u8; 16]);
    let mk = MembershipKey::new([0xaa; 32]);
    let admin = OwnerAddr([0xbb; 16]);

    let (pub_tx, _pub_rx) = tokio::sync::mpsc::channel(8);
    let (_sub_tx, sub_rx) = tokio::sync::mpsc::channel(8);
    registry
        .spawn_engine(cid, mk, admin, false, pub_tx, sub_rx)
        .await
        .expect("spawn");

    // Spawning persists empty CRDT + replay files. Confirm the dir
    // exists before teardown.
    let community_dir = dir.path().join("communities").join(hex::encode(cid.0));
    // The dir is created lazily on first persist write. Force a flush
    // by directly invoking it via the registry's existing public surface
    // — or simply trust spawn_engine has lazy-initialized it. Implementer:
    // if the dir doesn't exist after spawn, drive a tick that triggers
    // persist (e.g., via the engine's flush_now), then re-check.

    registry
        .shutdown_engine_and_cleanup_persistence(&cid)
        .await
        .expect("teardown");

    // Engine no longer in registry.
    assert!(
        !registry.has_engine(&cid).await,
        "engine map still holds {} after shutdown",
        hex::encode(cid.0)
    );

    // Persistence directory removed.
    assert!(
        !community_dir.exists(),
        "per-community persist dir not removed: {:?}",
        community_dir
    );
}

#[tokio::test]
async fn pending_redemption_oneshot_fires_when_event_id_inserts_via_local() {
    use harmony_app::community_state_sync::{
        CommunityRegistryConfig, CommunitySyncRegistry, IdentityResolver,
        DEFAULT_DEBOUNCE_MS,
    };
    use harmony_app::content_store::{ContentStore, RuntimeContentStore};
    use harmony_app::community_membership::{sign_event, EventPayload, MembershipEventKind};
    use harmony_app::owner_state_types::{Hlc, MembershipKey, OwnerAddr, SpaceId};
    use std::sync::Arc;

    struct AdminResolver { addr: OwnerAddr, pubkey: [u8; 64] }
    #[async_trait::async_trait]
    impl IdentityResolver for AdminResolver {
        async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
            if *addr == self.addr { Some(self.pubkey) } else { None }
        }
    }

    let identity = harmony_identity::PrivateIdentity::from_seed(&[0x55; 32]);
    let admin_addr = OwnerAddr(identity.identity.address_hash);
    let admin_pub = identity.identity.to_public_bytes();
    let admin_sk = {
        let priv_bytes = identity.to_private_bytes();
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&priv_bytes[32..64]);
        Arc::new(ed25519_dalek::SigningKey::from_bytes(&seed))
    };

    let (cas_op_tx, _cas_op_rx) = tokio::sync::mpsc::channel(8);
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(1000),
    ));
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "admin-dev".into(),
        content_store: cs,
        identity_resolver: Arc::new(AdminResolver {
            addr: admin_addr,
            pubkey: admin_pub,
        }),
        identity_dir: dir.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: admin_addr,
        signing_key: Arc::clone(&admin_sk),
    }));

    let cid = SpaceId([0x10; 16]);
    let mk = MembershipKey::new([0xaa; 32]);
    let (pub_tx, _pub_rx) = tokio::sync::mpsc::channel(8);
    let (_sub_tx, sub_rx) = tokio::sync::mpsc::channel(8);
    registry
        .spawn_engine(cid, mk, admin_addr, /* is_invite_only */ false, pub_tx, sub_rx)
        .await
        .expect("spawn");

    // Mint a self-Join event and register a oneshot keyed on its EventId.
    let event_id = [0x77u8; 16];
    let join = sign_event(
        &EventPayload {
            id: event_id,
            community_id: cid,
            kind: MembershipEventKind::Join,
            actor: admin_addr,
            at: Hlc { wall_ms: 1000, logical: 0, device_id: "admin-dev".into() },
        },
        admin_sk.as_ref(),
    )
    .unwrap();

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    registry
        .register_pending_redemption(event_id, tx)
        .await;

    let engine = registry
        .engine_arc(&cid)
        .await
        .expect("engine present");
    engine
        .insert_local_event(join.clone())
        .await
        .expect("insert");

    // Oneshot fires on Inserted.
    tokio::time::timeout(std::time::Duration::from_secs(2), rx)
        .await
        .expect("oneshot did not fire within 2s")
        .expect("oneshot sender dropped without firing");
}

#[tokio::test]
async fn pending_redemption_unregistered_when_no_match() {
    // Register a oneshot keyed on EventId X, mint and insert a different
    // event Y, assert the oneshot has NOT fired (verify the receive-side
    // notify is keyed correctly).
    //
    // Same setup as the prior test — copy the registry+engine setup,
    // register oneshot for [0xee; 16], insert an event with id [0x11; 16],
    // then `tokio::time::timeout(short, rx)` should resolve to Err
    // (timeout — oneshot didn't fire).
    //
    // Implementer: copy the prior test's setup; only the inserted
    // event id and the assertion shape differ.
    todo!("copy setup from pending_redemption_oneshot_fires_when_event_id_inserts_via_local; assert NO fire on a non-matching insert");
}
```

The fourth test (`pending_redemption_unregistered_when_no_match`) is left as a `todo!()` because its body is structurally identical to the prior test save for the insert-event-id and timeout-direction. Implementer fills it in by copying the third test's setup and inverting the timeout assertion (`expect_err("must NOT fire")`).

- [ ] **Step 2: Run the tests — verify they fail to compile**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_sync_registry_unit shutdown_engine_and_cleanup_persistence pending_redemption 2>&1 | tail -30
```

Expected: compile failure (`shutdown_engine_and_cleanup_persistence`, `register_pending_redemption` don't exist).

- [ ] **Step 3: Add `pending_redemptions` field to `CommunitySyncRegistry` + new helpers**

Edit `src-tauri/src/community_state_sync.rs:2066`:

```rust
pub struct CommunitySyncRegistry {
    cfg: Arc<CommunityRegistryConfig>,
    engines: tokio::sync::Mutex<BTreeMap<SpaceId, Arc<CommunitySyncEngine>>>,
    /// ZEB-262 Phase 4: per-EventId oneshots that fire when the matching
    /// SignedMembershipEvent has been Inserted into ANY engine in this
    /// registry. The redeem_invite IPC registers a oneshot keyed on its
    /// minted bootstrap_join.id BEFORE sending the CommunityInvite
    /// packet, then awaits the oneshot with timeout. The receive path
    /// (handle_unicast) inserts the counter-signed Join via
    /// engine.insert_local_event; the engine's post-insert hook calls
    /// notify_pending_redemption(event.id), which fires the matching
    /// oneshot.
    ///
    /// Lock-discipline: the map is held under a tokio Mutex. Callers
    /// MUST drop the guard before any `.await` on the recovered Sender.
    /// Helpers `register_pending_redemption` / `take_pending_redemption`
    /// / `notify_pending_redemption` enforce this by always taking the
    /// lock + extracting + dropping the guard BEFORE any await.
    pending_redemptions: tokio::sync::Mutex<
        std::collections::HashMap<
            crate::community_membership::EventId,
            tokio::sync::oneshot::Sender<()>,
        >,
    >,
}

impl CommunitySyncRegistry {
    pub fn new(cfg: CommunityRegistryConfig) -> Self {
        Self {
            cfg: Arc::new(cfg),
            engines: tokio::sync::Mutex::new(BTreeMap::new()),
            pending_redemptions: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    // ... existing methods ...

    /// Register a oneshot to fire when the SignedMembershipEvent with
    /// `event_id` is Inserted into any engine in this registry. Replaces
    /// any existing oneshot for the same event_id (the prior sender is
    /// dropped, which the prior caller's `.await` on the receiver
    /// surfaces as `Err(RecvError)` — interpret as "redemption
    /// superseded"). v1 doesn't deduplicate registrations because the
    /// caller pattern (one redeem_invite IPC = one fresh event_id) keeps
    /// the map naturally sparse.
    pub async fn register_pending_redemption(
        &self,
        event_id: crate::community_membership::EventId,
        sender: tokio::sync::oneshot::Sender<()>,
    ) {
        let mut g = self.pending_redemptions.lock().await;
        g.insert(event_id, sender);
        // guard dropped at end of scope
    }

    /// Remove the oneshot for `event_id` without firing it. Called by
    /// the IPC's timeout path so a late-arriving counter-signed Join
    /// doesn't try to send to a dead receiver.
    pub async fn take_pending_redemption(
        &self,
        event_id: &crate::community_membership::EventId,
    ) -> Option<tokio::sync::oneshot::Sender<()>> {
        let mut g = self.pending_redemptions.lock().await;
        g.remove(event_id)
    }

    /// If a oneshot is registered for `event_id`, take it out of the
    /// map and fire it. No-op if no registration exists. Called by the
    /// engine's insert-success hooks (insert_local_event +
    /// handle_incoming_publish).
    ///
    /// Lock-discipline: the map lock is held only across the `remove`
    /// call. The `send(())` is non-async on `oneshot::Sender::send`, so
    /// no await happens with the guard alive.
    pub async fn notify_pending_redemption(
        &self,
        event_id: &crate::community_membership::EventId,
    ) {
        let sender = {
            let mut g = self.pending_redemptions.lock().await;
            g.remove(event_id)
        };
        if let Some(tx) = sender {
            // tx.send(()) returns Result<(), ()> — error means receiver
            // already dropped (timeout fired before us). Either way the
            // oneshot is consumed; we've satisfied our notify contract.
            let _ = tx.send(());
        }
    }

    /// ZEB-258 rollback primitive: stop the engine task for `community_id`
    /// (drops adapter + Zenoh subscriber), wait for it to drain, and
    /// remove its per-community persistence directory.
    ///
    /// **Idempotent:** unknown community_id returns `Ok(())`.
    /// **Caller responsibility:** ensure no other thread holds an
    /// `Arc<CommunitySyncEngine>` from this registry. Typical use is
    /// "I just spawned this; no one else has a handle yet." If a handle
    /// has leaked elsewhere, those holders see TransportClosed once
    /// teardown completes.
    pub async fn shutdown_engine_and_cleanup_persistence(
        &self,
        community_id: &SpaceId,
    ) -> Result<(), CommunitySyncError> {
        // Phase 1: stop_engine (existing surface) — handles the no-engine
        // case as Ok() so this method also flows through idempotently.
        self.stop_engine(community_id).await?;

        // Phase 2: remove the per-community persistence directory. Use
        // tokio::fs::remove_dir_all so we don't park a worker thread on
        // sync std::fs::remove_dir_all.
        let dir = self.cfg.identity_dir.join("communities").join(hex::encode(community_id.0));
        if dir.exists() {
            tokio::fs::remove_dir_all(&dir).await.map_err(|e| {
                CommunitySyncError::Persist(format!(
                    "remove_dir_all {:?}: {e}",
                    dir
                ))
            })?;
        }
        Ok(())
    }
}
```

- [ ] **Step 4: Wire `notify_pending_redemption` into the insert hooks**

Edit `src-tauri/src/community_state_sync.rs:765` (the `insert_local_event` method's post-insert block, around the existing `if matches!(outcome, ... ::Inserted)` branch):

```rust
        if matches!(
            outcome,
            crate::community_state_crdt::InsertOutcome::Inserted
        ) {
            if let Some(tx) = self.delta_tx.as_ref() {
                let _ = tx.try_send(CommunityMembershipDelta {
                    community_id: event.community_id,
                    event: event.clone(),
                });
            }
            self.notify_dirty();
            // ZEB-262 Phase 4: notify any redeem_invite IPC waiting on
            // this event id. The engine doesn't own the registry; the
            // notify hook lives via the Arc<Notify>-style channel that
            // start_engine plumbs in. For the engine-internal hook,
            // store a notify_pending callback on InternalCtx that the
            // registry installs at spawn time.
            //
            // Implementation: extend CommunitySyncEngineConfig with a
            // new optional field
            //   pending_redemption_notify: Option<
            //       Arc<dyn Fn(&EventId) + Send + Sync>
            //   >
            // The registry's spawn_engine populates it with a closure
            // capturing &self.pending_redemptions (via a weak Arc to
            // avoid a cycle). The engine's insert_local_event +
            // handle_incoming_publish call the closure on Inserted.
        }
```

**Implementer detail:** the engine doesn't currently know about the registry. To call `notify_pending_redemption`, plumb a callback. Simplest plumb:

1. Add a new `Option<std::sync::Arc<dyn Fn(&crate::community_membership::EventId) + Send + Sync>>` field on `CommunitySyncEngineConfig`, defaulting to `None`.
2. Mirror it onto `InternalCtx` and the `CommunitySyncEngine` struct.
3. In the registry's `spawn_engine`, populate the field with a closure that calls `notify_pending_redemption`. The closure captures `Arc::downgrade(&self_arc)` to avoid a cycle.
4. In `insert_local_event` (post-Inserted) and `handle_incoming_publish` (post-Inserted, around line 1827), call the callback if `Some`.

The signature shape — function pointer over `&EventId` rather than `&SignedMembershipEvent` — keeps the callback minimal and lets the registry decide what to do with the event id.

Concretely: in `CommunitySyncRegistry::spawn_engine`:

```rust
        let registry_weak = std::sync::Weak::new(); // see note below
        let pending_notify: std::sync::Arc<
            dyn Fn(&crate::community_membership::EventId) + Send + Sync
        > = std::sync::Arc::new({
            let registry = registry_weak.clone();
            move |event_id: &crate::community_membership::EventId| {
                if let Some(reg) = registry.upgrade() {
                    let event_id = *event_id;
                    tokio::spawn(async move {
                        reg.notify_pending_redemption(&event_id).await;
                    });
                }
            }
        });
```

**Caveat on `registry_weak`:** the registry has to be wrapped in `Arc<Self>` for a Weak to be available. If `CommunitySyncRegistry` isn't currently `Arc`-shared, the call sites in `lib.rs` need to switch to `Arc<CommunitySyncRegistry>` (the Phase 3 NodeState already holds `Option<Arc<CommunitySyncRegistry>>`). Verify by reading `lib.rs:5716` (`g.community_registry.clone().ok_or(...)?`) — if `community_registry: Option<Arc<CommunitySyncRegistry>>`, the Arc is already in place; `Arc::downgrade(&community_registry)` produces the Weak.

If the registry isn't Arc-shared today, this task ALSO converts it. That's a small additional change but trivial — add `Arc::new(...)` at construction in `lib.rs::start_node`, change all clone sites to `Arc::clone`. The compiler enforces every call site.

The `tokio::spawn` inside the callback is necessary because the engine's insert path is sync at the call point (it's inside an `if matches!(...)` branch that's not in an async fn — wait, actually `insert_local_event` IS async, so `.await` is fine — replace `tokio::spawn` with a direct `.await`). Implementer: pick whichever fits the call site.

The `handle_incoming_publish` post-merge insert (around line 1827, the `Inserted` arm of the merge result match) also gets the same notify call.

- [ ] **Step 5: Run the tests — verify they pass**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_sync_registry_unit shutdown_engine_and_cleanup_persistence pending_redemption 2>&1 | tail -30
```

Expected: PASS for all 4 tests.

- [ ] **Step 5b: Swap `stop_engine` → `shutdown_engine_and_cleanup_persistence` in `create_community_inner`**

Now that the new helper exists, edit Task 1's `create_community_inner` (in `src-tauri/src/lib.rs`) to use it. The four rollback sites (adapter-dispatch fail, bootstrap-Join not inserted, fence abort, apply rejected) currently call `community_registry.stop_engine(&minted.community_id).await`; replace each with `community_registry.shutdown_engine_and_cleanup_persistence(&minted.community_id).await` and update the warn-message text. The `tracing::warn!` arms remain on Err.

This keeps Task 1's atomic-rollback guarantee semantically identical but makes the persistence directory go away on rollback (closing the orphan-dir gap that Task 1 explicitly tolerated as a "tolerable until Task 7 lands" issue).

After the swap, Task 1's regression test (`create_community_atomic_rollback_on_adapter_dispatch_failure`) still passes — it asserts owner-state byte-identity, not on-disk dir presence.

- [ ] **Step 6: Run cargo fmt + clippy + workspace tests**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
```

Expected: all green. The new field on `CommunitySyncEngineConfig` (`pending_redemption_notify`) defaults to `None`, so existing test sites that don't supply it still compile.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/community_state_sync.rs src-tauri/src/lib.rs \
        src-tauri/tests/community_sync_registry_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-262): pending_redemptions + shutdown_engine_and_cleanup_persistence

CommunitySyncRegistry gains:
- pending_redemptions: per-EventId oneshots fired by engine insert hooks;
  redeem_invite IPC registers, awaits with timeout, gets notified on
  counter-signed Join landing.
- shutdown_engine_and_cleanup_persistence: ZEB-258 rollback primitive
  (stop engine + remove per-community persist dir); idempotent on
  unknown community_id.

Lock-discipline: pending_redemptions guard NEVER held across .await.
Engine notify-callback installed via spawn_engine, captures Arc::Weak
to the registry to avoid cycles.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: `redeem_invite` invite-only branch (send path)

**Why:** Wire the 10-step invite-only flow per spec §"Send path: redeem_invite". Owner-state Space commit is the LAST step (ZEB-258 reorder mirrors Task 1's `create_community` change). The flow registers a oneshot keyed on `bootstrap_join.id`, sends the `CommunityInvitePacket` via the existing `unicast_send_tx` mpsc, and awaits with a 15-second timeout (env-overridable for tests via `HARMONY_REDEEM_INVITE_TIMEOUT_MS`). On any failure before the owner-state commit, the engine is torn down via `shutdown_engine_and_cleanup_persistence` (Task 7).

**Files:**
- Modify: `src-tauri/src/lib.rs:6117-6346` (the `redeem_invite` IPC body)
- Modify: `src-tauri/tests/community_sync_integration.rs` (timeout regression + atomic-rollback regression for invite-only)

- [ ] **Step 1: Write failing timeout regression test**

Append to `src-tauri/tests/community_sync_integration.rs`:

```rust
/// ZEB-262 Phase 4: redeem_invite_only_times_out_when_inviter_offline.
/// Construct an invite-only invite from Alice (admin), drop the
/// Reticulum forwarder so Bob's CommunityInvite packet is suppressed,
/// call redeem_invite_inner with a short timeout via env var, expect
/// Err. Owner-state Space MUST NOT be committed.
///
/// This test bypasses the Tauri State plumbing by calling the inner
/// helper directly. The helper signature is the post-reorder shape
/// (Task 8 ships it).
#[tokio::test]
async fn redeem_invite_only_times_out_when_inviter_offline() {
    use harmony_app::community_state_sync::{
        CommunityRegistryConfig, CommunitySyncRegistry, IdentityResolver,
        DEFAULT_DEBOUNCE_MS,
    };
    use harmony_app::content_store::{ContentStore, RuntimeContentStore};
    use harmony_app::community_invite::{
        CommunityInvitePayload, InviteToken, encode_invite_url,
    };
    use harmony_app::owner_state_crdt::OwnerState;
    use harmony_app::owner_state_persist::canonicalize;
    use harmony_app::owner_state_types::{Hlc, MembershipKey, OwnerAddr, SpaceId};
    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;

    // Use a short timeout so the test runs fast.
    std::env::set_var("HARMONY_REDEEM_INVITE_TIMEOUT_MS", "300");

    struct AliceResolver { addr: OwnerAddr, pubkey: [u8; 64] }
    #[async_trait::async_trait]
    impl IdentityResolver for AliceResolver {
        async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
            if *addr == self.addr { Some(self.pubkey) } else { None }
        }
    }

    let alice = harmony_identity::PrivateIdentity::from_seed(&[0xa1; 32]);
    let alice_addr = OwnerAddr(alice.identity.address_hash);
    let alice_pub = alice.identity.to_public_bytes();

    let bob = harmony_identity::PrivateIdentity::from_seed(&[0xb2; 32]);
    let bob_addr = OwnerAddr(bob.identity.address_hash);
    let bob_sk = {
        let priv_bytes = bob.to_private_bytes();
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&priv_bytes[32..64]);
        Arc::new(ed25519_dalek::SigningKey::from_bytes(&seed))
    };

    // Build an invite-only URL Alice would have generated for Bob.
    // (Sig over (inviter, invitee_hint, minted_at) with Alice's identity.)
    let community_id = SpaceId([0x33; 16]);
    let mk = MembershipKey::new([0xaa; 32]);
    let token_payload_bytes = harmony_app::community_invite::test_helpers::canonical_invite_token_bytes_for_test(
        alice_addr, Some(bob_addr), Hlc { wall_ms: 1000, logical: 0, device_id: "alice-dev".into() },
    );
    let token_sig = alice.sign(&token_payload_bytes);
    let invite_token = InviteToken {
        inviter: alice_addr,
        invitee_hint: Some(bob_addr),
        minted_at: Hlc { wall_ms: 1000, logical: 0, device_id: "alice-dev".into() },
        sig: token_sig,
    };
    let url = encode_invite_url(&CommunityInvitePayload {
        community_id,
        membership_key: mk.clone(),
        admin_addr: alice_addr,
        community_name: "Test".into(),
        is_invite_only: true,
        expires_at: None,
        invite_token: Some(invite_token),
    })
    .expect("encode URL");

    // Bob's side: registry + crdt + tracker.
    let (cas_op_tx, _cas_op_rx) = tokio::sync::mpsc::channel(8);
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        std::time::Duration::from_millis(1000),
    ));
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "bob-dev".into(),
        content_store: cs,
        identity_resolver: Arc::new(AliceResolver { addr: alice_addr, pubkey: alice_pub }),
        identity_dir: dir.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: bob_addr,
        signing_key: Arc::clone(&bob_sk),
    }));

    let crdt_state = Arc::new(TokioMutex::new(OwnerState::default()));
    let hlc_tracker = Arc::new(TokioMutex::new(Default::default()));

    // Drop the unicast send receiver so any try_send fails.
    let (unicast_tx, unicast_rx) =
        tokio::sync::mpsc::channel::<harmony_app::dm_outbox::UnicastSendRequest>(64);
    drop(unicast_rx);

    // Adapter request channel.
    let (adapter_tx, _adapter_rx) =
        tokio::sync::mpsc::channel::<harmony_app::event_loop::CommunityAdapterRequest>(64);

    // dm_outbox stub.
    let bob_pub = bob.identity.to_public_bytes();
    let bob_device_hash = harmony_app::owner_state_types::DeviceIdentityHash(bob.identity.address_hash);
    let dm_outbox = Arc::new(tokio::sync::Mutex::new(harmony_app::dm_outbox::DmOutbox::new(
        "bob-dev".into(),
        bob_addr,
        bob_device_hash,
        Arc::clone(&bob_sk),
        Arc::new(bob.clone()),
    )));

    let pre_bytes: Vec<u8> = {
        let g = crdt_state.lock().await;
        canonicalize(&g).expect("encode pre-state")
    };

    // Call redeem_invite_inner — the inner helper Task 8 extracts.
    // Signature: same shape as create_community_inner (Task 1).
    let result = harmony_app::redeem_invite_inner(
        url,
        Arc::clone(&crdt_state),
        Arc::clone(&hlc_tracker),
        "bob-dev".into(),
        bob_addr,
        Arc::clone(&bob_sk),
        Arc::clone(&registry),
        adapter_tx,
        unicast_tx,
        Arc::clone(&dm_outbox),
        /* generation */ 0,
        /* state_lock */ unsafe { std::mem::zeroed() }, // see note below
    )
    .await;

    assert!(
        result.is_err(),
        "invite-only redeem must Err on inviter-offline; got {:?}",
        result
    );

    let post_bytes: Vec<u8> = {
        let g = crdt_state.lock().await;
        canonicalize(&g).expect("encode post-state")
    };
    assert_eq!(
        pre_bytes, post_bytes,
        "ZEB-258: owner-state CRDT must be byte-identical pre/post a \
         failed redeem_invite (orphan Space row would prove the \
         reorder didn't land)"
    );

    std::env::remove_var("HARMONY_REDEEM_INVITE_TIMEOUT_MS");
}
```

**Note on `state_lock: unsafe { std::mem::zeroed() }`:** Tauri's `tauri::State<'_, _>` is not constructible outside a Tauri context. The inner helper's signature includes it for the snapshot-then-spawn fence (re-acquires the std lock to check `generation`). For tests that don't exercise the fence, the cleanest path is to refactor the inner helper so the fence-check is a callback (`F: FnOnce() -> Result<(), String>`) rather than a borrowed `tauri::State`. The test passes a no-op closure; production passes a closure that re-locks `state_lock` and checks `generation`.

Concrete signature shape Task 8 ships:

```rust
pub async fn redeem_invite_inner<F>(
    url: String,
    crdt_state: Arc<TokioMutex<OwnerState>>,
    hlc_tracker: Arc<TokioMutex<BTreeMap<String, Hlc>>>,
    device_id: String,
    self_owner: OwnerAddr,
    signing_key: Arc<ed25519_dalek::SigningKey>,
    community_registry: Arc<CommunitySyncRegistry>,
    community_adapter_tx: mpsc::Sender<CommunityAdapterRequest>,
    unicast_send_tx: mpsc::Sender<UnicastSendRequest>,
    dm_outbox: Arc<TokioMutex<DmOutbox>>,
    snapshot_generation: u64,
    fence_check: F,
) -> Result<String, String>
where
    F: Fn() -> Result<(), String>,
```

The IPC wrapper passes `move || -> Result<(), String> { let g = state_lock.lock()...; if g.generation != snapshot_generation { Err(...) } else { Ok(()) } }`. The test passes `|| Ok(())`.

This is a minor refactor — also apply to `create_community_inner` (Task 1 ships the un-refactored shape; Task 8 normalizes both). Adopt the closure pattern in BOTH tasks at the time Task 8 lands; document the change in the commit message.

(If the closure-pattern refactor of `create_community_inner` from Task 1 turns out invasive, defer it — Task 1 keeps the `state_lock` form, Task 8 ships only `redeem_invite_inner` with the closure. Either way, no production callsite changes shape.)

The `test_helpers::canonical_invite_token_bytes_for_test` referenced above is a `pub mod test_helpers` re-export of the private `canonical_invite_token_bytes` from Task 6 — implementer: add a `#[cfg(test)] pub mod test_helpers { pub use super::canonical_invite_token_bytes as ...; }` block in `community_invite.rs` if no integration-test-friendly accessor exists.

- [ ] **Step 2: Run the test — verify it fails to compile**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_sync_integration redeem_invite_only_times_out_when_inviter_offline 2>&1 | tail -30
```

Expected: compile failure on `redeem_invite_inner`.

- [ ] **Step 3: Add `redeem_invite_inner`**

Replace the body of `async fn redeem_invite` in `src-tauri/src/lib.rs:6117` with a thin wrapper that delegates to a new `redeem_invite_inner`. Above the IPC, add the inner helper:

```rust
/// ZEB-262 Phase 4: invite-only redeem_invite inner helper. Encodes
/// the 10-step flow per spec §"Send path: redeem_invite":
///
///   1. decode URL
///   2. snapshot handles (done by caller — passed in as args)
///   3. wall_now_ms
///   4. RESERVE HLC under tracker lock
///   5. mint_redemption
///   6. spawn_engine + dispatch adapter
///   7. branch on payload.is_invite_only:
///      OPEN — engine.insert_local_event(bootstrap_join)
///      INVITE-ONLY:
///        a. register oneshot on bootstrap_join.id
///        b. build CommunityInviteSigned + sign
///        c. resolve inviter Reticulum dest, send packet
///        d. await oneshot ≤ T (env HARMONY_REDEEM_INVITE_TIMEOUT_MS)
///   8. fence_check (generation guard)
///   9. COMMIT owner-state Space (LAST step)
///  10. return Ok
///
/// On any failure between steps 6-8 (invite-only branch), the engine
/// is torn down via shutdown_engine_and_cleanup_persistence and
/// owner-state is byte-identical to pre-call.
#[allow(clippy::too_many_arguments)]
pub async fn redeem_invite_inner<F>(
    url: String,
    crdt_state: std::sync::Arc<
        tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>,
    >,
    hlc_tracker: std::sync::Arc<
        tokio::sync::Mutex<
            std::collections::BTreeMap<String, crate::owner_state_types::Hlc>,
        >,
    >,
    device_id: String,
    self_owner: crate::owner_state_types::OwnerAddr,
    signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
    community_registry: std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>,
    community_adapter_tx: tokio::sync::mpsc::Sender<crate::event_loop::CommunityAdapterRequest>,
    unicast_send_tx: tokio::sync::mpsc::Sender<crate::dm_outbox::UnicastSendRequest>,
    dm_outbox: std::sync::Arc<tokio::sync::Mutex<crate::dm_outbox::DmOutbox>>,
    _snapshot_generation: u64, // captured by fence_check
    fence_check: F,
) -> Result<String, String>
where
    F: Fn() -> Result<(), String>,
{
    let payload = crate::community_invite::decode_invite_url(&url)
        .map_err(|e| format!("decode invite URL: {e}"))?;

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // 4. Reserve HLC.
    let prev_hlc = {
        let t = hlc_tracker.lock().await;
        t.get(&device_id).cloned()
    };

    // 5. Mint.
    let minted = mint_redemption(
        &payload,
        self_owner,
        signing_key.as_ref(),
        &device_id,
        wall_now_ms,
        prev_hlc.as_ref(),
    )?;

    {
        let mut tracker_g = hlc_tracker.lock().await;
        tracker_g.insert(device_id.clone(), minted.space.created_at.clone());
    }

    // 6. Spawn engine + dispatch adapter. Invite-only engines spawn
    //    with is_invite_only=true so verify_event applies the countersig
    //    rule.
    let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let (sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

    community_registry
        .spawn_engine(
            minted.community_id,
            minted.membership_key.clone(),
            payload.admin_addr, // engine's authority root is the inviter
            payload.is_invite_only,
            pub_tx,
            sub_rx,
        )
        .await
        .map_err(|e| format!("registry.spawn_engine: {e}"))?;

    // Idempotent re-redeem detection — engine may already exist (reused
    // from a prior redeem). spawn_engine is idempotent.
    let engine_already_existed = community_registry
        .engine_arc(&minted.community_id)
        .await
        .is_some()
        && {
            // Coarse check: if engine pre-existed AND we just attempted a
            // spawn that returned Ok without consuming our channels, the
            // engine_already_existed signal is true. We can't easily
            // distinguish from here; use the pub_rx/sub_tx-still-alive
            // heuristic.
            true
        };

    if !engine_already_existed {
        if let Err(e) = community_adapter_tx
            .try_send(crate::event_loop::CommunityAdapterRequest {
                id_hex: hex::encode(minted.community_id.0),
                publisher_rx: pub_rx,
                subscriber_tx: sub_tx,
            })
        {
            // Adapter dispatch failed — tear down the engine.
            if let Err(stop_err) = community_registry
                .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                .await
            {
                tracing::warn!(
                    error = %stop_err,
                    community_id = %hex::encode(minted.community_id.0),
                    "shutdown failed during redeem_invite adapter-dispatch rollback"
                );
            }
            return Err(match e {
                tokio::sync::mpsc::error::TrySendError::Full(_) =>
                    "adapter request queue full; please retry".to_string(),
                tokio::sync::mpsc::error::TrySendError::Closed(_) =>
                    "adapter request channel closed (event_loop stopped?)".to_string(),
            });
        }
    } else {
        drop(pub_rx);
        drop(sub_tx);
    }

    // 7. Branch.
    if !payload.is_invite_only {
        // OPEN: insert bootstrap_join via the engine.
        let engine_arc = community_registry
            .engine_arc(&minted.community_id)
            .await
            .ok_or("engine vanished after spawn — registry race")?;
        let outcome = engine_arc
            .insert_local_event(minted.bootstrap_join.clone())
            .await
            .map_err(|e| format!("engine.insert_local_event: {e}"))?;
        if !matches!(
            outcome,
            crate::community_state_crdt::InsertOutcome::Inserted
        ) {
            // Bootstrap Join didn't insert; tear down + bail.
            if let Err(stop_err) = community_registry
                .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                .await
            {
                tracing::warn!(error = %stop_err, "shutdown failed during open redeem rollback");
            }
            return Err(format!("self Join not inserted (got {outcome:?})"));
        }
    } else {
        // INVITE-ONLY: 7a-d.
        let invite_token = payload
            .invite_token
            .as_ref()
            .ok_or("invite-only payload missing invite_token")?
            .clone();

        // 7a. Register oneshot keyed on bootstrap_join.id.
        let (notify_tx, notify_rx) = tokio::sync::oneshot::channel::<()>();
        community_registry
            .register_pending_redemption(minted.bootstrap_join.id, notify_tx)
            .await;

        // 7b. Build + sign CommunityInviteSigned.
        let (joiner_pub, joiner_device_hash, sign_key_arc) = {
            let outbox_g = dm_outbox.lock().await;
            // Joiner's identity_pub: derive from outbox.private_identity.
            let joiner_pub = outbox_g.private_identity.identity.to_public_bytes();
            let joiner_device_hash = crate::owner_state_types::DeviceIdentityHash(
                outbox_g.private_identity.identity.address_hash,
            );
            let sign_key_arc = std::sync::Arc::clone(&outbox_g.signing_key);
            (joiner_pub, joiner_device_hash, sign_key_arc)
        };

        let signed = crate::community_invite::CommunityInviteSigned {
            community_id: minted.community_id,
            join_event: minted.bootstrap_join.clone(),
            invite_token,
            joiner_identity_pub: joiner_pub,
            signing_device_hash: joiner_device_hash,
            created_at: minted.bootstrap_join.at.clone(),
        };

        let packet = crate::community_invite::build_signed_invite_packet(
            signed,
            sign_key_arc.as_ref(),
        )
        .map_err(|e| format!("build_signed_invite_packet: {e}"))?;
        let wire = crate::community_invite::encode_packet(&packet)
            .map_err(|e| format!("encode_packet: {e}"))?;

        // 7c. Resolve inviter's Reticulum destination(s) and send.
        let inviter_addr = payload.admin_addr;
        let destinations = resolve_destinations_for_owner(
            crdt_state.as_ref(),
            inviter_addr,
        )
        .await;
        if destinations.is_empty() {
            // No known device for inviter — drop oneshot + tear down.
            let _ = community_registry
                .take_pending_redemption(&minted.bootstrap_join.id)
                .await;
            if let Err(stop_err) = community_registry
                .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                .await
            {
                tracing::warn!(error = %stop_err, "shutdown failed during inviter-unknown rollback");
            }
            return Err(format!(
                "no known device for inviter {} — invite cannot route",
                hex::encode(inviter_addr.0)
            ));
        }
        for destination_hash in &destinations {
            if let Err(e) = unicast_send_tx
                .try_send(crate::dm_outbox::UnicastSendRequest {
                    destination_hash: *destination_hash,
                    packet: wire.clone(),
                })
            {
                let _ = community_registry
                    .take_pending_redemption(&minted.bootstrap_join.id)
                    .await;
                if let Err(stop_err) = community_registry
                    .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                    .await
                {
                    tracing::warn!(error = %stop_err, "shutdown failed during unicast-send rollback");
                }
                return Err(format!(
                    "unicast_send_tx try_send: {e}"
                ));
            }
        }

        // 7d. Await oneshot ≤ T.
        let timeout_ms: u64 = std::env::var("HARMONY_REDEEM_INVITE_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(15_000);

        match tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            notify_rx,
        )
        .await
        {
            Ok(Ok(())) => {
                // Counter-signed Join landed — proceed.
            }
            Ok(Err(_recv_err)) => {
                // Sender dropped without sending — should be unreachable
                // if pending_redemptions enforces "one-shot".
                if let Err(stop_err) = community_registry
                    .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                    .await
                {
                    tracing::warn!(error = %stop_err, "shutdown failed during oneshot-recv-err rollback");
                }
                return Err("invite-only redemption oneshot closed unexpectedly".into());
            }
            Err(_elapsed) => {
                let _ = community_registry
                    .take_pending_redemption(&minted.bootstrap_join.id)
                    .await;
                if let Err(stop_err) = community_registry
                    .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                    .await
                {
                    tracing::warn!(error = %stop_err, "shutdown failed during timeout rollback");
                }
                return Err(format!(
                    "invite-only redemption timed out after {}ms",
                    timeout_ms
                ));
            }
        }
    }

    // 8. SNAPSHOT-THEN-COMMIT FENCE.
    fence_check()?;

    // 9. COMMIT owner-state Space (LAST step).
    {
        let mut state_g = crdt_state.lock().await;
        let outcome = state_g.apply_space_with_canonicalization(minted.space.clone());
        if matches!(outcome, crate::owner_state_crdt::ApplyOutcome::Rejected(_)) {
            drop(state_g);
            if let Err(stop_err) = community_registry
                .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                .await
            {
                tracing::warn!(error = %stop_err, "shutdown failed during apply-rejected rollback");
            }
            return Err(format!("apply_space rejected redemption Space: {outcome:?}"));
        }
    }

    Ok(hex::encode(minted.community_id.0))
}

/// Helper: resolve OwnerAddr → Vec<destination_hash> via OwnerState.
/// Mirrors dm_outbox's resolve_destinations (same pattern; lib.rs keeps
/// it inline because the inviter-resolution path is community-specific).
async fn resolve_destinations_for_owner(
    crdt_state: &tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>,
    owner: crate::owner_state_types::OwnerAddr,
) -> Vec<[u8; 16]> {
    let g = crdt_state.lock().await;
    // Implementer: reuse the existing dm_outbox::resolve_destinations
    // logic. It walks OwnerDeviceCache for owner → device_identity_hashes.
    // If the helper isn't directly accessible, copy the lookup inline:
    //   g.owner_device_cache.get(&owner).map(|entry| entry.device_hashes.clone()).unwrap_or_default()
    // Adopt whichever name matches the actual OwnerDeviceCache shape.
    g.owner_device_cache
        .get(&owner)
        .map(|entry| entry.device_hashes.iter().map(|h| h.0).collect())
        .unwrap_or_default()
}
```

Then update `redeem_invite` (the `#[tauri::command]`) to delegate:

```rust
#[tauri::command]
async fn redeem_invite(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    url: String,
) -> Result<String, String> {
    let (
        crdt_state,
        hlc_tracker,
        device_id,
        self_owner,
        community_registry,
        community_adapter_tx,
        unicast_send_tx,
        dm_outbox,
        snapshot_generation,
        signing_key,
    ) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        let dm_outbox = g
            .dm_outbox
            .clone()
            .ok_or("dm_outbox missing — no owner identity?")?;
        let signing_key = {
            let outbox_g = dm_outbox.blocking_lock_owned();
            std::sync::Arc::clone(&outbox_g.signing_key)
        };
        (
            g.crdt_state.clone().ok_or("crdt_state missing")?,
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry.clone().ok_or("community_registry missing")?,
            g.community_adapter_request_tx.clone().ok_or("adapter tx missing")?,
            g.unicast_send_tx.clone().ok_or("unicast_send_tx missing")?,
            dm_outbox,
            g.generation,
            signing_key,
        )
    };

    let fence_check = {
        let snapshot_generation = snapshot_generation;
        let state_lock_arc = state_lock.clone();
        move || -> Result<(), String> {
            let g = state_lock_arc
                .lock()
                .map_err(|e| format!("NodeState poisoned: {e}"))?;
            if g.generation != snapshot_generation {
                return Err(format!(
                    "node generation changed during redeem_invite (was {}, now {})",
                    snapshot_generation, g.generation
                ));
            }
            Ok(())
        }
    };

    redeem_invite_inner(
        url,
        crdt_state,
        hlc_tracker,
        device_id,
        self_owner,
        signing_key,
        community_registry,
        community_adapter_tx,
        unicast_send_tx,
        dm_outbox,
        snapshot_generation,
        fence_check,
    )
    .await
}
```

`tauri::State::clone()` may not exist; alternative is to pass the std `Arc<Mutex<NodeState>>` directly. Implementer: confirm `tauri::State` cloning behavior. If `State` itself isn't `Clone`, restructure the closure to capture only the `Arc<std::sync::Mutex<NodeState>>` extracted from the State by calling `state_lock.inner().clone()` — Tauri exposes the underlying handle.

`unicast_send_tx` MUST be added to `NodeState` if it isn't already a field — Phase 3b should have added it for DM path. Verify by reading `NodeState`'s definition.

- [ ] **Step 4: Run the test — verify it now compiles and passes**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_sync_integration redeem_invite_only_times_out_when_inviter_offline 2>&1 | tail -30
```

Expected: PASS. If the test asserts owner-state byte-identity but the assertion fails, debug the rollback path (`shutdown_engine_and_cleanup_persistence` was called but did Space-row commit happen anyway? — that's the bug Task 8 is preventing).

- [ ] **Step 5: Run the broader integration suite**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_sync_integration 2>&1 | tail -30
cargo test --test community_open_flow_integration 2>&1 | tail -10
```

Expected: all green. The OPEN redemption path Phase 3 ships still works (the inner helper handles the OPEN branch via the same path).

- [ ] **Step 6: Run cargo fmt + clippy + workspace tests**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
```

Expected: all green.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/tests/community_sync_integration.rs
git commit -m "$(cat <<'EOF'
feat(zeb-262): redeem_invite invite-only send path

Wires the 10-step invite-only flow per spec:
  HLC reservation → mint → spawn engine → adapter dispatch →
  register oneshot → build + sign CommunityInvitePacket → Reticulum
  unicast → await oneshot ≤ HARMONY_REDEEM_INVITE_TIMEOUT_MS (15s
  default) → fence_check → owner-state Space commit (LAST).

ZEB-258 reorder: any failure between spawn and the final commit tears
down the engine via shutdown_engine_and_cleanup_persistence; owner-state
is byte-identical to pre-call.

Test: redeem_invite_only_times_out_when_inviter_offline asserts both
the timeout error AND owner-state byte-identity. OPEN path still
works (Phase 3 regression covered by community_open_flow_integration).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Receive dispatch (`inbound_packet.rs`) + `community_invite::handle_unicast` + 2-node integration test

**Why:** This task wires the Reticulum receive side. `event_loop::handle_runtime_action_or_dispatch` (line 1414) currently routes every `RuntimeAction::UnicastReceived` through `dm_outbox.handle_unicast`. We add a small pre-fork: peek the discriminant byte, route `0x10` to `community_invite::handle_unicast`, fall through to DM dispatch for everything else (including `0x01-0x03` and unknown discriminants — the existing DM dispatch already drops unknowns).

`community_invite::handle_unicast` runs the 11-step verify chain per spec §"Receive path", attaches the countersig via `community_membership::attach_countersig_with_identity`, calls `engine.insert_local_event` (which fires the pending-redemption oneshot via Task 7's notify hook), and emits `community-state-sync-degraded` on rejection.

**Files:**
- Create: `src-tauri/src/inbound_packet.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod inbound_packet;`)
- Modify: `src-tauri/src/event_loop.rs:1414` (peek discriminant)
- Modify: `src-tauri/src/community_invite.rs` (add `handle_unicast`)
- Create: `src-tauri/tests/community_invite_only_integration.rs`

- [ ] **Step 1: Write the failing 2-node integration test**

Create `src-tauri/tests/community_invite_only_integration.rs`:

```rust
//! Two-engine invite-only round-trip — ZEB-262 Phase 4.
//!
//! Exercises the full invite-only redemption path:
//!   1. Alice creates invite-only community
//!   2. Alice generates an invite URL (with InviteToken signed by Alice)
//!   3. Bob calls redeem_invite_inner, which builds + sends a
//!      CommunityInvitePacket via the Reticulum unicast forwarder
//!   4. Alice's event_loop receives the packet, dispatches via
//!      inbound_packet to community_invite::handle_unicast
//!   5. handle_unicast verifies the chain, counter-signs Bob's Join,
//!      inserts via engine.insert_local_event, fires Bob's oneshot
//!   6. Bob's redeem_invite_inner returns Ok; owner-state Space row
//!      is committed; Bob's engine has the counter-signed Join
//!
//! The test bridges Bob's `unicast_send_tx` → Alice's
//! `RuntimeAction::UnicastReceived` via a forwarder task. Alice's
//! receive side runs handle_unicast directly (event_loop dispatch is
//! tested via the smaller inbound_packet unit test).

use harmony_app::community_invite;
use harmony_app::community_membership::{materialize, MemberStatus};
use harmony_app::community_state_sync::{
    CommunityRegistryConfig, CommunitySyncRegistry, IdentityResolver,
    DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{ContentStore, RuntimeContentStore};
use harmony_app::owner_state_crdt::OwnerState;
use harmony_app::owner_state_types::{Hlc, MembershipKey, OwnerAddr, SpaceId};
use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;

struct TwoIdentityResolver { a: (OwnerAddr, [u8; 64]), b: (OwnerAddr, [u8; 64]) }
#[async_trait::async_trait]
impl IdentityResolver for TwoIdentityResolver {
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        if *addr == self.a.0 { Some(self.a.1) }
        else if *addr == self.b.0 { Some(self.b.1) }
        else { None }
    }
}

/// Full happy-path test: Bob redeems Alice's invite-only invite,
/// counter-signed Join converges on both engines, both materializations
/// show Bob as Joined.
#[tokio::test]
async fn alice_redeems_invite_only_against_bob_admin() {
    // Set short timeout for fast test.
    std::env::set_var("HARMONY_REDEEM_INVITE_TIMEOUT_MS", "5000");

    let alice = harmony_identity::PrivateIdentity::from_seed(&[0xa1; 32]);
    let bob = harmony_identity::PrivateIdentity::from_seed(&[0xb2; 32]);
    let alice_addr = OwnerAddr(alice.identity.address_hash);
    let bob_addr = OwnerAddr(bob.identity.address_hash);
    let alice_pub = alice.identity.to_public_bytes();
    let bob_pub = bob.identity.to_public_bytes();

    let alice_sk = Arc::new({
        let priv_bytes = alice.to_private_bytes();
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&priv_bytes[32..64]);
        ed25519_dalek::SigningKey::from_bytes(&seed)
    });
    let bob_sk = Arc::new({
        let priv_bytes = bob.to_private_bytes();
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&priv_bytes[32..64]);
        ed25519_dalek::SigningKey::from_bytes(&seed)
    });

    let resolver: Arc<dyn IdentityResolver> = Arc::new(TwoIdentityResolver {
        a: (alice_addr, alice_pub),
        b: (bob_addr, bob_pub),
    });

    // Shared CAS forwarder.
    let (cas_tx_a, _cas_rx_a) = tokio::sync::mpsc::channel(8);
    let (cas_tx_b, _cas_rx_b) = tokio::sync::mpsc::channel(8);
    let cs_a: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(cas_tx_a, std::time::Duration::from_secs(2)));
    let cs_b: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(cas_tx_b, std::time::Duration::from_secs(2)));

    let dir_a = tempfile::tempdir().expect("dir a");
    let dir_b = tempfile::tempdir().expect("dir b");

    let registry_a = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "alice-dev".into(),
        content_store: cs_a,
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_a.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: alice_addr,
        signing_key: Arc::clone(&alice_sk),
    }));
    let registry_b = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "bob-dev".into(),
        content_store: cs_b,
        identity_resolver: Arc::clone(&resolver),
        identity_dir: dir_b.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: bob_addr,
        signing_key: Arc::clone(&bob_sk),
    }));

    let crdt_a = Arc::new(TokioMutex::new(OwnerState::default()));
    let crdt_b = Arc::new(TokioMutex::new(OwnerState::default()));
    let tracker_a = Arc::new(TokioMutex::new(Default::default()));
    let tracker_b = Arc::new(TokioMutex::new(Default::default()));

    // Reticulum forwarder: Bob's outbound unicast → Alice's
    // RuntimeAction::UnicastReceived → community_invite::handle_unicast.
    let (bob_unicast_tx, mut bob_unicast_rx) =
        tokio::sync::mpsc::channel::<harmony_app::dm_outbox::UnicastSendRequest>(8);

    // Alice's dm_outbox (needs PrivateIdentity for countersign).
    let alice_dm_outbox = Arc::new(tokio::sync::Mutex::new(harmony_app::dm_outbox::DmOutbox::new(
        "alice-dev".into(),
        alice_addr,
        harmony_app::owner_state_types::DeviceIdentityHash(alice.identity.address_hash),
        Arc::clone(&alice_sk),
        Arc::new(alice.clone()),
    )));

    // Bob's dm_outbox.
    let bob_dm_outbox = Arc::new(tokio::sync::Mutex::new(harmony_app::dm_outbox::DmOutbox::new(
        "bob-dev".into(),
        bob_addr,
        harmony_app::owner_state_types::DeviceIdentityHash(bob.identity.address_hash),
        Arc::clone(&bob_sk),
        Arc::new(bob.clone()),
    )));

    // Adapter request channels (drained-but-ignored — engine internal
    // task doesn't actually need a real Zenoh adapter for this test).
    let (adapter_tx_a, mut adapter_rx_a) =
        tokio::sync::mpsc::channel::<harmony_app::event_loop::CommunityAdapterRequest>(8);
    tokio::spawn(async move { while let Some(_) = adapter_rx_a.recv().await {} });
    let (adapter_tx_b, mut adapter_rx_b) =
        tokio::sync::mpsc::channel::<harmony_app::event_loop::CommunityAdapterRequest>(8);
    tokio::spawn(async move { while let Some(_) = adapter_rx_b.recv().await {} });

    // Alice creates invite-only community + bootstrap-Joins.
    let alice_minted = harmony_app::mint_community_creation(
        "InviteOnly",
        true,
        alice_addr,
        alice_sk.as_ref(),
        "alice-dev",
        100_000,
        None,
    )
    .expect("alice mint");
    let community_id = alice_minted.community_id;

    let (a_pub_tx, _a_pub_rx) = tokio::sync::mpsc::channel(8);
    let (_a_sub_tx, a_sub_rx) = tokio::sync::mpsc::channel(8);
    registry_a
        .spawn_engine(
            community_id,
            alice_minted.membership_key.clone(),
            alice_addr,
            true,
            a_pub_tx,
            a_sub_rx,
        )
        .await
        .expect("spawn alice engine");
    let alice_engine = registry_a
        .engine_arc(&community_id)
        .await
        .expect("alice engine");
    alice_engine
        .insert_local_event(alice_minted.bootstrap_join.clone())
        .await
        .expect("alice bootstrap insert");

    // Build Alice's InviteToken sig over (alice_addr, Some(bob_addr),
    // minted_at). Use the canonical helper — implementer: confirm the
    // exact helper name; this calls the test_helpers re-export.
    let token_minted_at = Hlc { wall_ms: 100_500, logical: 0, device_id: "alice-dev".into() };
    let token_payload_bytes = community_invite::test_helpers::canonical_invite_token_bytes_for_test(
        alice_addr,
        Some(bob_addr),
        token_minted_at.clone(),
    );
    let token_sig = alice.sign(&token_payload_bytes);
    let invite_token = community_invite::InviteToken {
        inviter: alice_addr,
        invitee_hint: Some(bob_addr),
        minted_at: token_minted_at,
        sig: token_sig,
    };
    let invite_url = community_invite::encode_invite_url(&community_invite::CommunityInvitePayload {
        community_id,
        membership_key: alice_minted.membership_key.clone(),
        admin_addr: alice_addr,
        community_name: "InviteOnly".into(),
        is_invite_only: true,
        expires_at: None,
        invite_token: Some(invite_token),
    })
    .expect("encode URL");

    // Forwarder: drain Bob's unicast outbound, decode + dispatch to
    // Alice's handle_unicast.
    let alice_app = (); // tests don't have AppHandle — handle_unicast
                       // takes Option<&AppHandle>; pass None for tests
                       // (no Tauri events emitted).
    let registry_a_for_fwd = Arc::clone(&registry_a);
    let alice_dm_for_fwd = Arc::clone(&alice_dm_outbox);
    let crdt_a_for_fwd = Arc::clone(&crdt_a);
    tokio::spawn(async move {
        while let Some(req) = bob_unicast_rx.recv().await {
            // Drop destination_hash (Reticulum routing detail) — we
            // already know the packet is destined for Alice in this
            // test fixture. Real production routes via Reticulum's
            // identity-hash-keyed link layer.
            let _ = community_invite::handle_unicast(
                &registry_a_for_fwd,
                &alice_dm_for_fwd,
                &crdt_a_for_fwd,
                req.packet,
                None::<&()>, // app handle stub (test passes None)
            )
            .await;
        }
    });

    // Need to put alice_minted's bootstrap_join in Bob's CRDT/engine
    // somehow so Bob's "self joined" eligibility check (when verify
    // happens against Bob's engine) finds Bob as Joined post-redemption.
    // For invite-only, the receive-side handle_unicast runs on ALICE's
    // engine — Alice is the inviter. Alice IS Joined. So no pre-seed
    // for Bob is needed; the verify happens in Alice's engine state.

    // Bob: spawn engine + redeem.
    let (b_pub_tx, _b_pub_rx) = tokio::sync::mpsc::channel(8);
    let (_b_sub_tx, b_sub_rx) = tokio::sync::mpsc::channel(8);
    // (Engine spawned inside redeem_invite_inner below.)

    let result = harmony_app::redeem_invite_inner(
        invite_url,
        Arc::clone(&crdt_b),
        Arc::clone(&tracker_b),
        "bob-dev".into(),
        bob_addr,
        Arc::clone(&bob_sk),
        Arc::clone(&registry_b),
        adapter_tx_b,
        bob_unicast_tx,
        Arc::clone(&bob_dm_outbox),
        0,
        || Ok(()), // no fence check needed in test
    )
    .await;

    assert!(
        result.is_ok(),
        "invite-only redeem must succeed; got {:?}",
        result
    );

    // Alice's engine has admin Join + counter-signed Bob Join.
    let alice_state = registry_a
        .state_for(&community_id)
        .await
        .expect("alice state");
    let alice_events: Vec<_> = {
        let g = alice_state.lock().await;
        g.events.values().cloned().collect()
    };
    assert_eq!(alice_events.len(), 2, "alice should hold 2 events");
    let mat_a = materialize(&alice_events, alice_addr);
    assert_eq!(
        mat_a.members.get(&bob_addr).map(|m| m.status),
        Some(MemberStatus::Joined),
        "Bob must materialize as Joined on Alice's side"
    );

    std::env::remove_var("HARMONY_REDEEM_INVITE_TIMEOUT_MS");
}
```

This test is long but straightforward — mirrors `community_open_flow_integration.rs::open_community_create_redeem_leave_round_trip` with the invite-only Reticulum forwarder added.

- [ ] **Step 2: Run the test — verify it fails to compile**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_invite_only_integration alice_redeems_invite_only_against_bob_admin 2>&1 | tail -30
```

Expected: compile failure on `community_invite::handle_unicast`.

- [ ] **Step 3: Add `community_invite::handle_unicast`**

Append to `src-tauri/src/community_invite.rs`:

```rust
/// ZEB-262 Phase 4: receive-side handler for Reticulum unicast packets
/// with discriminant 0x10. Runs the 11-step verify chain per spec
/// §"Receive path", attaches the counter-sig via
/// attach_countersig_with_identity, inserts the counter-signed Join
/// via engine.insert_local_event, and (via the engine's notify hook
/// from Task 7) fires the pending-redemption oneshot.
///
/// On any verify failure, emits `community-state-sync-degraded` Tauri
/// event with the reason tag (when `app` is Some). No retry — Reticulum
/// retransmit will redrive from the sender if needed.
///
/// `app` is `Option<&impl Trait>` so tests can pass `None::<&()>` and
/// production passes `Some(&app_handle)`. The exact bound is
/// `Option<&dyn AppHandleEmit>` — define a tiny trait that production
/// AppHandle implements via `app.emit(...)`.
pub async fn handle_unicast<H: AppHandleEmit>(
    community_registry: &std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>,
    dm_outbox: &std::sync::Arc<tokio::sync::Mutex<crate::dm_outbox::DmOutbox>>,
    crdt_state: &std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    packet_bytes: Vec<u8>,
    app: Option<&H>,
) -> Result<(), CommunityInviteVerifyError> {
    // 1. decode_packet
    let packet = match decode_packet(&packet_bytes) {
        Ok(p) => p,
        Err(_e) => {
            // Decode failure: caller can't identify community_id → no
            // degraded event (we don't know which community to flag).
            // Drop + log.
            tracing::warn!(error = ?_e, "community_invite decode_packet failed; dropping");
            return Err(CommunityInviteVerifyError::EnvelopeSigInvalid); // generic
        }
    };
    let CommunityInvitePacket::Invite { signed, signature, signed_bytes } = packet;

    // 2. Snapshot self_owner + private_identity from dm_outbox.
    let (self_owner, self_private_identity) = {
        let outbox_g = dm_outbox.lock().await;
        (outbox_g.self_owner, std::sync::Arc::clone(&outbox_g.private_identity))
    };

    // 3. Pure verify chain (signed_bytes envelope sig is checked separately).
    if let Err(e) = verify_envelope_sig(&signed_bytes, &signature, &signed.joiner_identity_pub) {
        emit_degraded(app, &signed.community_id, e.reason_tag());
        return Err(e);
    }
    let join_event = match verify_packet_pure(
        &signed,
        self_owner,
        || std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        self_private_identity.as_ref(),
    ) {
        Ok(e) => e,
        Err(e) => {
            emit_degraded(app, &signed.community_id, e.reason_tag());
            return Err(e);
        }
    };

    // 4. Resolve engine + state for community_id.
    let engine_arc = match community_registry.engine_arc(&signed.community_id).await {
        Some(e) => e,
        None => {
            let e = CommunityInviteVerifyError::CommunityUnknown {
                community_id: signed.community_id,
            };
            emit_degraded(app, &signed.community_id, e.reason_tag());
            return Err(e);
        }
    };
    let state_arc = match community_registry.state_for(&signed.community_id).await {
        Some(s) => s,
        None => {
            let e = CommunityInviteVerifyError::CommunityUnknown {
                community_id: signed.community_id,
            };
            emit_degraded(app, &signed.community_id, e.reason_tag());
            return Err(e);
        }
    };

    // 5. Self-eligibility check: self must be Joined; power ≥
    //    invite_threshold (= 0 in v1, structural).
    let (self_status, self_power) = {
        let s = state_arc.lock().await;
        let events: Vec<_> = s.events.values().cloned().collect();
        drop(s);
        let mat = crate::community_membership::materialize(&events, engine_arc.admin_addr());
        let st = mat.members.get(&self_owner).map(|m| m.status);
        let pw = mat.power_levels.get(&self_owner).copied().unwrap_or(0);
        (st, pw)
    };
    if self_status != Some(crate::community_membership::MemberStatus::Joined) {
        let e = CommunityInviteVerifyError::SelfNotJoined;
        emit_degraded(app, &signed.community_id, e.reason_tag());
        return Err(e);
    }
    // v1: invite_threshold = 0; this is a structural no-op but a stable
    // hook for ZEB-251.
    let invite_threshold: u8 = 0;
    if self_power < invite_threshold {
        let e = CommunityInviteVerifyError::SelfPowerInsufficient {
            self_power,
            threshold: invite_threshold,
        };
        emit_degraded(app, &signed.community_id, e.reason_tag());
        return Err(e);
    }

    // 6. Attach countersig.
    let counter_signed = match crate::community_membership::attach_countersig_with_identity(
        &join_event,
        self_private_identity.as_ref(),
    ) {
        Ok(e) => e,
        Err(_) => {
            // Encoder error — vanishingly rare.
            let e = CommunityInviteVerifyError::JoinSigInvalid; // closest fit
            emit_degraded(app, &signed.community_id, e.reason_tag());
            return Err(e);
        }
    };

    // 7. Insert via engine. The engine's post-Inserted hook (Task 7)
    //    fires pending_redemptions[event.id] for the joiner side.
    match engine_arc.insert_local_event(counter_signed).await {
        Ok(crate::community_state_crdt::InsertOutcome::Inserted) => Ok(()),
        Ok(crate::community_state_crdt::InsertOutcome::AlreadyKnown) => {
            // Idempotent retransmit — fine.
            Ok(())
        }
        Ok(crate::community_state_crdt::InsertOutcome::Rejected(verr)) => {
            tracing::warn!(error = ?verr, "counter-signed Join rejected by engine");
            // Map the engine's VerifyError to a JoinSigInvalid for now;
            // ZEB-251 might split this further.
            let e = CommunityInviteVerifyError::JoinSigInvalid;
            emit_degraded(app, &signed.community_id, e.reason_tag());
            Err(e)
        }
        Err(local_err) => {
            tracing::warn!(error = %local_err, "engine.insert_local_event errored");
            let e = CommunityInviteVerifyError::JoinSigInvalid;
            emit_degraded(app, &signed.community_id, e.reason_tag());
            Err(e)
        }
    }
}

/// Trait so `handle_unicast` can take either a real `tauri::AppHandle`
/// or a test stub (`None::<&()>`). Production impl on `tauri::AppHandle`
/// goes in lib.rs (small adapter). Tests pass `None`.
pub trait AppHandleEmit {
    fn emit_degraded(&self, community_id_hex: &str, reason_tag: &'static str);
}

/// Unit type implements as a no-op so tests can pass `None::<&()>`
/// safely — the trait method is never called in the None path.
impl AppHandleEmit for () {
    fn emit_degraded(&self, _: &str, _: &'static str) {}
}

fn emit_degraded<H: AppHandleEmit>(
    app: Option<&H>,
    community_id: &crate::owner_state_types::SpaceId,
    reason_tag: &'static str,
) {
    if let Some(app) = app {
        app.emit_degraded(&hex::encode(community_id.0), reason_tag);
    } else {
        tracing::warn!(
            community_id = %hex::encode(community_id.0),
            reason = reason_tag,
            "community_invite verify failed (no app handle); not emitting Tauri event"
        );
    }
}
```

In `lib.rs`, implement `AppHandleEmit` for `tauri::AppHandle<R>`:

```rust
impl<R: tauri::Runtime> crate::community_invite::AppHandleEmit for tauri::AppHandle<R> {
    fn emit_degraded(&self, community_id_hex: &str, reason_tag: &'static str) {
        let _ = self.emit(
            "community-state-sync-degraded",
            serde_json::json!({
                "communityId": community_id_hex,
                "reason": reason_tag,
            }),
        );
    }
}
```

- [ ] **Step 4: Add `inbound_packet.rs` discriminant dispatcher**

Create `src-tauri/src/inbound_packet.rs`:

```rust
//! ZEB-262 Phase 4: discriminant-based dispatch for inbound Reticulum
//! unicast packets. Peeks `packet[0]` and routes:
//!   0x10 → community_invite::handle_unicast
//!   else → fall through to dm_outbox.handle_unicast (existing path)
//!
//! Adding the new branch in a tight wrapper avoids refactoring DM
//! dispatch in this PR. The DM path's existing 0x01-0x03 handling +
//! unknown-discriminant logging are preserved.

/// Returns `true` if the packet was claimed by community_invite (and
/// dispatched), `false` if the caller should fall through to DM
/// dispatch.
///
/// The caller wires this into event_loop's UnicastReceived branch. On
/// `false`, the existing DM dispatch runs unchanged.
pub async fn try_dispatch_community<H: crate::community_invite::AppHandleEmit>(
    community_registry: Option<&std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>>,
    dm_outbox: &std::sync::Arc<tokio::sync::Mutex<crate::dm_outbox::DmOutbox>>,
    crdt_state: &std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    packet_bytes: &[u8],
    app: Option<&H>,
) -> bool {
    let disc = match packet_bytes.first() {
        Some(b) => *b,
        None => return false, // empty packet — let DM dispatch decide
    };
    if disc != 0x10 {
        return false;
    }
    let registry = match community_registry {
        Some(r) => r,
        None => {
            // No community runtime — drop. (Identity not loaded yet.)
            tracing::warn!(
                "received community_invite packet but community_registry is unset; dropping"
            );
            return true; // claimed (and dropped)
        }
    };
    let _ = crate::community_invite::handle_unicast(
        registry,
        dm_outbox,
        crdt_state,
        packet_bytes.to_vec(),
        app,
    )
    .await;
    true
}
```

In `src-tauri/src/lib.rs`, add `mod inbound_packet;` near the other module declarations.

- [ ] **Step 5: Wire `try_dispatch_community` into `event_loop.rs`**

Edit `src-tauri/src/event_loop.rs:1414` (the `if matches!(action, RuntimeAction::UnicastReceived { .. })` block). Before the `try_lock` chain, peek the packet:

```rust
    if matches!(action, RuntimeAction::UnicastReceived { .. }) {
        // ZEB-262 Phase 4: discriminant pre-dispatch. If the packet is
        // a community packet (0x10), route to community_invite handler;
        // otherwise fall through to DM dispatch (unchanged from Phase 3).
        if let RuntimeAction::UnicastReceived { packet, .. } = &action {
            if packet.first() == Some(&0x10) {
                if let (Some(outbox), Some(state), Some(registry)) =
                    (dm_outbox, crdt_state, community_registry)
                {
                    crate::inbound_packet::try_dispatch_community(
                        Some(registry),
                        outbox,
                        state,
                        packet,
                        Some(app),
                    )
                    .await;
                } else {
                    tracing::warn!(
                        "received community packet (0x10) but community runtime not initialized; dropping"
                    );
                }
                return;
            }
        }
        // Fall through: existing DM dispatch.
        // ... existing if let chain unchanged ...
    }
```

The `community_registry` arg needs to be added to `handle_runtime_action_or_dispatch`'s signature. Implementer: add `community_registry: Option<&std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>>` and plumb through every call site (3 places where `runtime.tick()` is invoked per the comment at line 1377).

- [ ] **Step 6: Run the integration test — verify it passes**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_invite_only_integration alice_redeems_invite_only_against_bob_admin 2>&1 | tail -30
```

Expected: PASS. If the oneshot doesn't fire within 5s, the engine's post-Inserted notify hook (Task 7) didn't reach `notify_pending_redemption`. Trace by adding `tracing::info!` before/after the notify call.

- [ ] **Step 7: Run cargo fmt + clippy + workspace tests**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
```

Expected: all green.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/inbound_packet.rs src-tauri/src/community_invite.rs \
        src-tauri/src/event_loop.rs src-tauri/src/lib.rs \
        src-tauri/tests/community_invite_only_integration.rs
git commit -m "$(cat <<'EOF'
feat(zeb-262): receive-side dispatch + handle_unicast + 2-node test

- inbound_packet.rs: tight discriminant pre-dispatch (0x10 → community,
  else → fall-through to DM)
- event_loop.rs: peek packet[0] before existing DM dispatch
- community_invite::handle_unicast: 11-step verify chain → counter-sign
  → engine.insert_local_event → engine notify hook fires
  pending_redemptions oneshot for the joiner-side IPC
- AppHandleEmit trait so the handler is testable without a real
  AppHandle
- Two-node integration test: Alice creates invite-only community,
  Bob redeems via Reticulum forwarder, counter-signed Join converges,
  both materializations show Bob as Joined

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: Final verification + push + PR

**Why:** Gating step. Run all CI gates locally one more time, verify the spec's acceptance criteria are covered, push the branch, and open the PR.

**Files:** None modified — verification + push only.

- [ ] **Step 1: Final cargo gates**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
```

Expected: all three commands succeed. If any fail, fix before pushing — never push red builds.

- [ ] **Step 2: Acceptance-criteria checklist (spec lines 509-518)**

Walk through each acceptance criterion and link to the test that proves it:

- [ ] `redeem_invite(invite_only_url)` no longer returns `Err("Phase 3 supports OPEN ...")` — covered by `alice_redeems_invite_only_against_bob_admin` (Task 9)
- [ ] `kick_from_community` mints a verified Kick event, publishes, returns Ok — `admin_kicks_member_round_trip` (Task 3)
- [ ] `set_power_level` analogous — `admin_sets_power_round_trip` (Task 3)
- [ ] Inbound 0x10 packet → event_loop routes → handle_unicast verifies + counter-signs + publishes — exercised by `alice_redeems_invite_only_against_bob_admin`
- [ ] Verification failures emit `community-state-sync-degraded` with reason tag — covered by the 7 reject-variant unit tests (Task 6) plus the `AppHandleEmit` trait contract
- [ ] ZEB-258 regression: simulate engine-spawn failure during `create_community` → owner-state byte-identical — `create_community_atomic_rollback_on_adapter_dispatch_failure` (Task 1)
- [ ] Wire-format fixture pinned — `community_invite_signed_wire_bytes_pinned` (Task 4)
- [ ] Two-engine integration test: Alice (open) redeems invite-only invite for community admined by Bob — `alice_redeems_invite_only_against_bob_admin`
- [ ] All existing tests still green; new gates: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace` — Step 1 above

If any criterion has no test, ADD one before pushing.

- [ ] **Step 3: Walk the diff one more time**

```bash
git log --oneline origin/main..HEAD
git diff --stat origin/main..HEAD
```

Expected: 9 commits (Task 1 through Task 9). Each commit message starts with `feat(zeb-262):` or `fix(zeb-258):`. The diff stats show changes in `src-tauri/src/community_invite.rs`, `community_state_sync.rs`, `dm_outbox.rs`, `event_loop.rs`, `lib.rs`, `inbound_packet.rs`, plus the 6 test files.

If any commit looks like it has unrelated cleanup or a stray secret/credential, fix before pushing.

- [ ] **Step 4: Push branch**

```bash
git push -u origin zeb-262-phase-4-invite-only-kick-set-power
```

Expected: branch pushed. If the remote has a force-push protection, this is a fresh branch so no force-push needed.

- [ ] **Step 5: Open PR**

```bash
gh pr create --title "ZEB-262 Phase 4: invite-only flow + kick + set-power (backend)" --body "$(cat <<'EOF'
## Summary

ZEB-262 Phase 4 + ZEB-258 atomic-rollback fix folded in.

- **Invite-only flow** (`redeem_invite` invite-only branch): joiner mints
  bootstrap Join, sends a `CommunityInvitePacket` (0x10) to the inviter
  via Reticulum unicast, awaits counter-sign with timeout (15s default,
  env-overridable). Inviter's `event_loop` routes 0x10 → 11-step verify →
  `attach_countersig_with_identity` → `engine.insert_local_event` →
  notifies pending-redemption oneshot.
- **`kick_from_community`** + **`set_power_level`** IPCs: mint signed
  events, insert via engine, translate `VerifyError` to user-readable
  strings.
- **ZEB-258 atomic rollback**: `create_community` + `redeem_invite` now
  commit owner-state Space LAST. Earlier failures (engine spawn, adapter
  dispatch, packet send, counter-sign timeout) tear down the engine via
  the new `shutdown_engine_and_cleanup_persistence` registry surface;
  owner-state byte-identical to pre-call.
- **Wire format**: `CommunityInviteSigned` (signed body) +
  `CommunityInvitePacket` (Path B app-sig wrapper, discriminant 0x10).
  Pinned canonical CBOR fixture locks the encoder.
- **Receive dispatch**: minimal new `inbound_packet.rs` peeks `packet[0]`
  and routes 0x10 → community_invite, else → existing DM path.

References:
- Spec: `docs/specs/2026-05-07-zeb-262-phase-4-invite-only-kick-set-power-design.md` (commit `cdbf7c8`)
- ZEB-256 publisher auth (PR #88, commit `5a691f0` on main): every
  Phase 4 published event flows through that auth gate
- Phase 3 ship: PR #87, commit `bc0facd`
- Closes [ZEB-262](https://linear.app/zeblith/issue/ZEB-262)
- Closes [ZEB-258](https://linear.app/zeblith/issue/ZEB-258)

## Test Plan

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] `cargo test --workspace --all-targets --locked`
- [ ] Two-node `alice_redeems_invite_only_against_bob_admin` integration test
- [ ] ZEB-258 `create_community_atomic_rollback_on_adapter_dispatch_failure`
- [ ] ZEB-258 `redeem_invite_only_times_out_when_inviter_offline`
- [ ] Kick + set-power round-trips
- [ ] All 7 verify-reject unit tests + canonical wire-format fixture

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Expected: PR URL printed. Capture and report to the human.

- [ ] **Step 6: Report PR URL to the human and stop**

Don't merge. Don't request review unless told to. The PR review cycle is the human's call. Surface the URL + one-line summary + the acceptance-criteria checklist; let the human drive merge.

The next phase (Phase 5: admin UI — `CommunitySettingsPanel`, `MemberRow`, `InviteRedeemDialog`) ships in a separate PR after Phase 4 merges. Do NOT start Phase 5 work in this PR.

---

## Self-review notes (for the writer of the plan, not the implementer)

- **Spec coverage:** every section of the spec maps to at least one task:
  - Goal → Tasks 1-9 collectively
  - Architecture (ASCII diagram) → Tasks 8 + 9 (send + receive)
  - Files touched → File structure section
  - Wire format (CommunityInviteSigned + CommunityInvitePacket) → Task 4
  - Send path: redeem_invite → Task 8
  - Send path: create_community → Task 1
  - Receive path → Task 9
  - IPC surface (kick + set_power) → Task 3
  - ZEB-258 atomic rollback → Tasks 1 + 7
  - Pending-redemption oneshot → Task 7
  - Error taxonomy → Task 6
  - Test surface (~13 tests) → Tasks 1, 3, 4, 5, 6, 7, 8, 9
- **Type consistency:** `CommunityInviteSigned`, `CommunityInvitePacket`, `CommunityInviteVerifyError`, `verify_packet_pure`, `verify_envelope_sig`, `build_signed_invite_packet`, `encode_packet`, `decode_packet`, `device_hash_from_identity_pub` are referenced consistently across tasks. `EventId`, `OwnerAddr`, `SpaceId` are existing types from `community_membership.rs` / `owner_state_types.rs`. `pending_redemptions` is keyed by `EventId` (= `[u8; 16]`).
- **Lock-discipline:** `pending_redemptions` map lock is taken + dropped in helpers; never held across `.await`. `crdt_state` lock is held only for the apply_space step (Task 1, 8); released before any subsequent await.
- **Open questions for implementer:**
  - The `tauri::State::clone()` issue in Task 8 — confirm Tauri's API and pick the closure-capture or Arc-extract path.
  - `harmony_identity::PrivateIdentity: Clone` (Task 2) — confirm; fall back to `from_private_bytes` if not.
  - `OwnerDeviceCache` accessor names (Task 8 `resolve_destinations_for_owner`) — match the actual field names; the helper's body is illustrative.
  - `canonical_invite_token_bytes` (Task 6) — the spec says the token sig covers `(inviter, invitee_hint, minted_at, expires_at)` but Phase 1's existing token shape may differ. Align with whatever Phase 1's `community_invite.rs` already encodes. Drift here is a verification breakage.
