# ZEB-875: claimant-bound atomic single-use invite claim — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a community invite genuinely single-use across *distinct* actors while letting the legitimate claimant always retry — by binding each invite token's single-use claim to the first actor that commits a `PendingJoin` for it, enforced atomically under the membership engine's insert lock.

**Architecture:** The claim is a *pure predicate over already-persisted `PendingJoin` CRDT events* (`invite_token.sig → first verified actor`). It is enforced by a new `LocalInsertPrecheck` variant that runs under the same `state.lock()` guard that appends the event (the exact `insert_local_channel_create` precedent), so check-and-insert is indivisible and durable-by-derivation. No wire-protocol change, no new persisted structure, no CRDT retraction.

**Tech Stack:** Rust, tokio, `cargo nextest`, `thiserror`. Design spec: `docs/superpowers/specs/2026-08-06-zeb-875-invite-claimant-bound-claim-design.md`.

## Global Constraints

- Rust MSRV **1.91**; frontend Node 20+ (no frontend change expected here).
- Cargo from `src-tauri/`. Gates (all must pass before PR): `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`; `npx tsc --noEmit` + `npx vitest run` (repo root).
- `--locked` and `--all-targets` are load-bearing. Include `--features test-fixtures` for integration tests.
- The claim binds on the **signature-verified** actor only (never a client-asserted one) — the gate lives *after* `verify_packet_pure`, *before* the insert.
- No leaked internal/debug string reaches a user (ZEB-872 honest-phrasing discipline). The rejection is host-side telemetry; the losing joiner sees the existing generic `"inviter_unreachable"` outcome (no wire change).
- `OwnerAddr` is treated as `Copy` below; if the compiler disagrees, add `.clone()` at the two marked sites.

---

## File Structure

- **Modify** `src-tauri/src/community_state_sync.rs` — add `LocalInsertError::InviteAlreadyClaimed`, `LocalInsertPrecheck::NoConflictingClaimantForInvite` + its `run` arm, and the `insert_local_claim_bound_pending_join` method. Engine-layer tests in this file's test module. (Task 1)
- **Modify** `src-tauri/src/community_invite.rs` — add `CommunityInviteVerifyError::InviteAlreadyClaimed` + its `reason_tag` arm; route the `handle_unicast` PendingJoin branch through the new engine method and map the engine error; add the legacy-branch unreachability `warn`. Integration tests in this file's test module. (Task 2)
- **Modify** `src-tauri/tests/pkarr_net/pkarr_iroh_redeem_full_integration.rs` — same-actor-retry e2e regression. (Task 3)

---

### Task 1: Engine — atomic claimant-bound claim seam + engine error

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs` (`LocalInsertError` enum ~`:775`; `LocalInsertPrecheck` enum + impl ~`:826`; new method near `insert_local_channel_create` ~`:1624`)
- Test: `src-tauri/src/community_state_sync.rs` test module (mirror the existing engine tests that exercise `insert_local_channel_create` / `insert_local_event*`)

**Interfaces:**
- Produces: `CommunitySyncEngine::insert_local_claim_bound_pending_join(event, invite_token_sig: [u8;64], claimant: OwnerAddr) -> Result<InsertOutcome, LocalInsertError>` and `LocalInsertError::InviteAlreadyClaimed { winner: OwnerAddr }` — consumed by Task 2.

- [ ] **Step 1: Write the failing engine tests.**

In the `community_state_sync.rs` test module, add tests. Build the engine + insert a `PendingJoin` exactly as the existing engine tests do (reuse their community/owner/event fixtures; a `PendingJoin` fixture is an invite-only `SignedMembershipEvent` with `kind: MembershipEventKind::PendingJoin { invite_token }` where `invite_token.sig` is the key). The claim logic:

```rust
// A claims sig S -> Inserted; a DIFFERENT actor B on S -> InviteAlreadyClaimed{winner:A};
// A again on S -> AlreadyKnown (idempotent); a DIFFERENT sig -> Inserted.
#[tokio::test]
async fn claim_bound_pending_join_rejects_a_second_distinct_actor() {
    let (engine, /* fixtures */ ..) = /* build engine + insert admin bootstrap, as existing tests do */;
    let sig = [0x11u8; 64];
    let a_evt = /* PendingJoin for actor A with invite_token.sig == sig */;
    let b_evt = /* PendingJoin for actor B with invite_token.sig == sig */;

    let r1 = engine.insert_local_claim_bound_pending_join(a_evt.clone(), sig, a_addr).await;
    assert!(matches!(r1, Ok(InsertOutcome::Inserted)), "first claimant wins: {r1:?}");

    let r2 = engine.insert_local_claim_bound_pending_join(b_evt, sig, b_addr).await;
    assert!(matches!(r2, Err(LocalInsertError::InviteAlreadyClaimed { winner }) if winner == a_addr),
        "distinct actor rejected with winner=A: {r2:?}");

    let r3 = engine.insert_local_claim_bound_pending_join(a_evt, sig, a_addr).await;
    assert!(matches!(r3, Ok(InsertOutcome::AlreadyKnown)), "same actor idempotent: {r3:?}");
}

#[tokio::test]
async fn claim_bound_pending_join_allows_a_different_token() {
    // A claims sig S1; B claims sig S2 -> both Inserted (claim keys on the token, not the actor).
}

// Concurrency: exactly one of two distinct-actor claims on the same sig wins.
// Structure ported from pkarr_invite_publisher.rs:404 (try_consume_friend_token_exactly_one_winner_under_concurrency).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claim_bound_pending_join_exactly_one_winner_under_concurrency() {
    let engine = Arc::new(/* built engine */);
    let sig = [0x55u8; 64];
    let a = { let e = Arc::clone(&engine); tokio::spawn(async move {
        matches!(e.insert_local_claim_bound_pending_join(a_evt, sig, a_addr).await, Ok(InsertOutcome::Inserted)) }) };
    let b = { let e = Arc::clone(&engine); tokio::spawn(async move {
        matches!(e.insert_local_claim_bound_pending_join(b_evt, sig, b_addr).await, Ok(InsertOutcome::Inserted)) }) };
    let (won_a, won_b) = tokio::join!(a, b);
    assert!(won_a.unwrap() ^ won_b.unwrap(), "exactly one distinct-actor claim wins the race");
}

// Restart-safety: verdict is a function of persisted events, not in-memory state.
#[tokio::test]
async fn claim_survives_engine_rebuild_from_persisted_events() {
    // 1. engine1: A claims sig S -> Inserted. 2. persist + drop engine1.
    // 3. rebuild engine2 from the persisted event log (as the boot/restore path does).
    // 4. engine2.insert_local_claim_bound_pending_join(b_evt, S, b_addr) -> InviteAlreadyClaimed{winner:A}.
}
```

- [ ] **Step 2: Run the tests — expect FAIL** (method + variant don't exist).
  Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(claim_bound_pending_join) + test(claim_survives_engine_rebuild)'`
  Expected: compile error / unresolved `insert_local_claim_bound_pending_join` + `LocalInsertError::InviteAlreadyClaimed`.

- [ ] **Step 3: Add the `LocalInsertError` variant.** In the `LocalInsertError` enum (`community_state_sync.rs` ~`:775`, `thiserror`, per-variant `#[error(...)]`):

```rust
/// ZEB-875: a claim-bound single-use invite token already has a committed
/// PendingJoin from a *different* actor; a distinct claimant is refused.
#[error("invite token already claimed by a different actor")]
InviteAlreadyClaimed { winner: crate::owner_state_types::OwnerAddr },
```

- [ ] **Step 4: Add the precheck variant + `run` arm.** In `LocalInsertPrecheck` (~`:826`):

```rust
// enum LocalInsertPrecheck { ... existing UniqueLiveChannelName ...
NoConflictingClaimantForInvite { invite_token_sig: [u8; 64], claimant: crate::owner_state_types::OwnerAddr },
```

In `impl LocalInsertPrecheck::run` (runs under the `state.lock()` guard, synchronous, no `.await`):

```rust
LocalInsertPrecheck::NoConflictingClaimantForInvite { invite_token_sig, claimant } => {
    // Scan RAW committed events (not materialized roster): permanent, robust to
    // a later Leave/Kick or the 30-day PendingJoin materialize expiry.
    for ev in state.events() {
        if let crate::community_membership::MembershipEventKind::PendingJoin { invite_token } = &ev.kind {
            if invite_token.sig == *invite_token_sig && ev.actor != *claimant {
                return Err(LocalInsertError::InviteAlreadyClaimed { winner: ev.actor }); // .clone() if OwnerAddr !Copy
            }
        }
    }
    Ok(())
}
```

- [ ] **Step 5: Add the public method.** Near `insert_local_channel_create` (~`:1624`), mirroring it exactly:

```rust
/// ZEB-875: atomic claimant-bound single-use invite insert. Under the one
/// `state` lock, refuses the insert if a PendingJoin for this invite token's
/// sig already exists from a *different* actor (returns InviteAlreadyClaimed);
/// a same-actor re-insert is the usual idempotent AlreadyKnown.
pub async fn insert_local_claim_bound_pending_join(
    &self,
    event: crate::community_membership::SignedMembershipEvent,
    invite_token_sig: [u8; 64],
    claimant: crate::owner_state_types::OwnerAddr,
) -> Result<crate::community_state_crdt::InsertOutcome, LocalInsertError> {
    if event.community_id != self.community_id {
        return Err(LocalInsertError::WrongCommunity {
            expected: self.community_id,
            got: event.community_id,
        });
    }
    self.insert_event_with_resolved_pubs(
        event,
        Some(LocalInsertPrecheck::NoConflictingClaimantForInvite { invite_token_sig, claimant }),
    )
    .await
}
```

- [ ] **Step 6: Run the tests — expect PASS.**
  Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(claim_bound_pending_join) + test(claim_survives_engine_rebuild)'`

- [ ] **Step 7: Scoped gate + commit.**
  Run: `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` then `cargo fmt --all`.
  ```bash
  git add src-tauri/src/community_state_sync.rs
  git commit -m "ZEB-875: engine — atomic claimant-bound single-use invite claim seam"
  ```

---

### Task 2: Redeem-path integration in `handle_unicast` + verify-error mapping

**Files:**
- Modify: `src-tauri/src/community_invite.rs` (`CommunityInviteVerifyError` enum ~`:1281`–`:1366` + `reason_tag` ~`:1391`; `handle_unicast` PendingJoin branch ~`:2275`–`:2320`; legacy branch ~`:2321`)
- Test: `src-tauri/src/community_invite.rs` test module (reuse the existing `handle_unicast` / signed-invite-packet fixtures)

**Interfaces:**
- Consumes: `insert_local_claim_bound_pending_join`, `LocalInsertError::InviteAlreadyClaimed` (Task 1).
- Produces: `CommunityInviteVerifyError::InviteAlreadyClaimed` — the surfaced verify error.

- [ ] **Step 1: Write the failing integration tests.** In the `community_invite.rs` test module, using the existing helpers that build a signed invite packet + a live registry/engine for a community:

```rust
// Two DISTINCT joiners redeem the SAME invite token via handle_unicast:
// first Ok, second Err(InviteAlreadyClaimed). A same-actor second call -> Ok (retry).
#[tokio::test]
async fn handle_unicast_rejects_a_second_distinct_actor_on_one_token() {
    let (registry, community_id, invite_token, ..) = /* mint invite-only community; build helpers */;
    let bob_packet = /* encode_packet of a PendingJoin invite for Bob using invite_token */;
    let carol_packet = /* same invite_token, actor = Carol */;

    let r1 = handle_unicast::<()>(&registry, &dm_outbox, &crdt_state, bob_packet.clone(), None).await;
    assert!(r1.is_ok(), "first distinct claimant joins: {r1:?}");

    let r2 = handle_unicast::<()>(&registry, &dm_outbox, &crdt_state, carol_packet, None).await;
    assert!(matches!(r2, Err(CommunityInviteVerifyError::InviteAlreadyClaimed)),
        "second distinct actor rejected: {r2:?}");

    let r3 = handle_unicast::<()>(&registry, &dm_outbox, &crdt_state, bob_packet, None).await;
    assert!(r3.is_ok(), "same-actor retry still succeeds (idempotent re-delivery): {r3:?}");
}
```

- [ ] **Step 2: Run — expect FAIL** (`InviteAlreadyClaimed` verify variant missing; branch still uses the old insert).
  Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(handle_unicast_rejects_a_second_distinct_actor)'`

- [ ] **Step 3: Add the verify-error variant + `reason_tag` arm.** In `CommunityInviteVerifyError` (~`:1366`, before the closing `}`):

```rust
/// ZEB-875: this invite's single-use token was already claimed by a
/// different actor's committed PendingJoin.
#[error("invite already claimed by another actor")]
InviteAlreadyClaimed,
```

In `impl CommunityInviteVerifyError::reason_tag` (the exhaustive match ~`:1391` — arm is mandatory or it won't compile):

```rust
Self::InviteAlreadyClaimed => "community_invite_already_claimed",
```

- [ ] **Step 4: Route the PendingJoin branch through the claim seam.** In the `is_pending_join_shape` branch (~`:2295`), capture the claimant before the move and swap the insert call:

```rust
let joiner_identity_pub = signed.joiner_identity_pub;
let claimant = join_event.actor;                 // ZEB-875: verified actor, captured before move
let _ = joiner_identity_pub;                      // (identity pubs are discarded post-ZEB-339)
match engine_arc
    .insert_local_claim_bound_pending_join(join_event, signed.invite_token.sig, claimant)
    .await
{
    Ok(crate::community_state_crdt::InsertOutcome::Inserted) => { /* unchanged: burn now lives in acceptor */ Ok(()) }
    Ok(crate::community_state_crdt::InsertOutcome::AlreadyKnown) => Ok(()),
    Ok(crate::community_state_crdt::InsertOutcome::Rejected(verr)) => {
        tracing::warn!(error = ?verr, "ZEB-254 handle_unicast: PendingJoin rejected by engine");
        let e = CommunityInviteVerifyError::EngineRejected;
        emit_degraded(app, &signed.community_id, e.reason_tag());
        Err(e)
    }
    Err(crate::community_state_sync::LocalInsertError::InviteAlreadyClaimed { winner }) => {
        tracing::warn!(?winner, ?claimant, "ZEB-875 handle_unicast: invite token already claimed by a different actor; rejecting redeem");
        let e = CommunityInviteVerifyError::InviteAlreadyClaimed;
        emit_degraded(app, &signed.community_id, e.reason_tag());
        Err(e)
    }
    Err(local_err) => {
        tracing::warn!(error = %local_err, "ZEB-254 handle_unicast: insert PendingJoin failed");
        let e = CommunityInviteVerifyError::EngineLocalError;
        emit_degraded(app, &signed.community_id, e.reason_tag());
        Err(e)
    }
}
```

(Keep `joiner_identity_pub` bound only if still referenced elsewhere in the branch; otherwise drop the now-unused binding to avoid a clippy warning — the `let _ =` line above is a placeholder, remove it and the binding if nothing else uses the pub.)

- [ ] **Step 5: Add the legacy-branch unreachability marker.** In the `else` (legacy `Join`) branch (~`:2321`), before the `attach_countersig_with_device_key` call, add:

```rust
// ZEB-875: a claimant-bound single-use invite is enforced only on the
// PendingJoin shape (the only shape any live minter emits — lib.rs:39511).
// A bare legacy Join carries no invite_token, so it cannot be claim-bound;
// reaching here means a pre-ZEB-254 stale client. Not a live single-use path.
debug_assert!(false, "ZEB-875: legacy bare-Join redeem is not claim-bound (stale pre-ZEB-254 client)");
tracing::warn!("ZEB-875: invite redeem via legacy bare-Join path (no claim binding; pre-ZEB-254 client)");
```

- [ ] **Step 6: Run the integration tests — expect PASS.**
  Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(handle_unicast_rejects_a_second_distinct_actor)'`

- [ ] **Step 7: Scoped gate + commit.**
  Run clippy (as Task 1 Step 7) + `cargo fmt --all`.
  ```bash
  git add src-tauri/src/community_invite.rs
  git commit -m "ZEB-875: handle_unicast — route invite redeem through the claimant-bound claim"
  ```

---

### Task 3: E2E regression — same-actor retry over real transport still joins

**Files:**
- Modify/Test: `src-tauri/tests/pkarr_net/pkarr_iroh_redeem_full_integration.rs` (mirror `bob_joins_alice_via_iroh_handshake_option_a` ~`:749` and the redeem call ~`:867`)

**Interfaces:**
- Consumes: the full production redeem path with the claim now in it (Tasks 1–2). No new production API.

- [ ] **Step 1: Write the e2e regression test.** Drive Bob's redeem twice against Alice's host and assert both reach `"joined"` (the claim must not break the happy path or same-actor retry over the real iroh handshake):

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_actor_retry_still_joins_under_claim_binding() {
    let s = setup_two_party_iroh_handshake().await;
    let invite_url = /* mint from s.alice_minted as the existing tests do */;

    let first = /* connectivity_redeem_invite_iroh_inner(...) as at :867, with s.bob_* deps */;
    assert_eq!(first.status, "joined", "first redeem joins: {:?}", first.status);

    let second = /* identical redeem call, same Bob identity + same invite_url */;
    assert_eq!(second.status, "joined",
        "same-actor retry is idempotent under the claim (claim keyed on Bob, not consumed away): {:?}", second.status);

    // Bob is Joined exactly once.
    let bob_mat = /* materialize s.registry_bob engine as at :1010 */;
    assert_eq!(bob_mat.members.get(&s.bob_addr).map(|m| m.status), Some(MemberStatus::Joined));
}
```

> **Scope note (explicit, not silent):** the distinct-actor *network* race (a second joiner over iroh) is intentionally NOT added — the rejection is enforced in `handle_unicast`/the engine (covered by Tasks 1–2) and is transport-independent, and the losing racer only ever sees the generic `"inviter_unreachable"` status (no wire change). A network Carol would add large harness setup for zero new enforcement coverage. Same judgment ZEB-874 used to avoid a racy transport-failure test.

- [ ] **Step 2: Run — expect PASS** (production change already in place from Tasks 1–2).
  Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(same_actor_retry_still_joins_under_claim_binding)'`

- [ ] **Step 3: Full local gate.**
  ```bash
  cd src-tauri && cargo fmt --all -- --check \
    && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings \
    && cargo nextest run --locked --workspace --all-targets --features test-fixtures
  cd .. && npx tsc --noEmit && npx vitest run
  ```

- [ ] **Step 4: Commit.**
  ```bash
  git add src-tauri/tests/pkarr_net/pkarr_iroh_redeem_full_integration.rs
  git commit -m "ZEB-875: e2e — same-actor invite retry still joins under claim binding"
  ```

---

## Self-review

- **Spec coverage:** claim rule → Task 1 precheck; after-verify enforcement → Task 2 Step 4 (gate placed after `verify_packet_pure`, claimant = verified `join_event.actor`); atomicity/durability → Task 1 precheck under the `state` lock + restart test; error surface (host-side telemetry, generic joiner status) → Task 2 (verify variant + warn) with no wire change; both branches → Task 2 Steps 4–5; testing 1–6 → Tasks 1–3.
- **Placeholders:** production code is verbatim from source extraction; test *fixtures* reference the concrete existing helpers to mirror (engine tests, signed-invite-packet builders, the two-party harness) with exact assertions — appropriate for an implementer with repo access.
- **Type consistency:** `insert_local_claim_bound_pending_join` / `LocalInsertError::InviteAlreadyClaimed { winner }` / `CommunityInviteVerifyError::InviteAlreadyClaimed` names match across Tasks 1–2; `reason_tag` arm added (exhaustive match).
